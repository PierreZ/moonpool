//! Provider parity for the `DirectIo::Required` contract.
//!
//! The simulator exists to test the code that ships, which only holds while
//! the two `StorageProvider` implementations *mean* the same thing. The
//! direction that matters is one-sided: a simulated provider that is more
//! permissive than the production one lets simulated code come to depend on
//! an open production refuses, and the run stays green until it reaches a real
//! filesystem.
//!
//! `DirectIo::Required` is where that gap would open, because the two backends
//! reach the contract from opposite sides — production defers the lifecycle
//! behind a real `O_DIRECT` open, the simulator settles the capability before
//! it touches its namespace at all. So the scenarios below are written once
//! and run against both.

use moonpool_core::{DirectIo, OpenOptions, StorageFile, StorageProvider, TokioStorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration};
use std::net::IpAddr;
use tempfile::TempDir;

fn test_ip() -> IpAddr {
    "127.0.0.1".parse().expect("valid IP")
}

fn local_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime")
}

/// Run one storage scenario, stepping the world until the task finishes.
async fn run_storage_test<F, Fut, T>(mut sim: SimWorld, f: F) -> T
where
    F: FnOnce(moonpool_sim::SimStorageProvider) -> Fut,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let provider = sim.storage_provider(test_ip());
    let handle = tokio::spawn(f(provider));

    while !handle.is_finished() {
        while sim.pending_event_count() > 0 {
            sim.step();
        }
        tokio::task::yield_now().await;
    }

    handle.await.expect("task panicked")
}

/// What one step did, in terms both backends can report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observation {
    /// The open succeeded, and this is what the file says about itself.
    Opened { direct: bool },
    /// The open failed with this error kind.
    Refused(std::io::ErrorKind),
    /// Whether the path exists once the step is over.
    Exists(bool),
}

/// What a step *allowed*, which is the whole of the one-sided invariant.
///
/// The error kind is deliberately not part of this. Where the platform has no
/// direct I/O at all, `Required` is refused before the path is ever resolved,
/// so a missing file is reported as `Unsupported`; where it does, the path is
/// resolved first and a missing file is reported as missing. Both refuse, and
/// which reason surfaces first is a property of the platform rather than of
/// the contract.
fn permitted(observation: Observation) -> bool {
    match observation {
        Observation::Opened { .. } => true,
        Observation::Refused(_) => false,
        Observation::Exists(exists) => exists,
    }
}

fn observe<F: StorageFile>(result: std::io::Result<F>) -> Observation {
    match result {
        Ok(file) => Observation::Opened {
            direct: file.is_direct_io(),
        },
        Err(error) => Observation::Refused(error.kind()),
    }
}

/// The five `Required` scenarios, plus what each left on the filesystem.
///
/// Every step is labelled, so a divergence names the scenario rather than an
/// index. `prefix` is what a path has to be prefixed with to land in this
/// run's own directory: the production provider gets a temporary directory,
/// the simulator a namespace of its own.
async fn required_contract<P: StorageProvider>(
    provider: P,
    prefix: String,
) -> std::io::Result<Log> {
    let mut log = Vec::new();

    // The bootstrap `Required` will not do for you: an ordinary create, made
    // durable, before the capability is required of the file that now exists.
    let existing = format!("{prefix}existing.db");
    let bootstrap = provider
        .open(&existing, OpenOptions::create_new_write())
        .await?;
    bootstrap.sync_all().await?;
    drop(bootstrap);

    log.push((
        "existing + Required",
        observe(
            provider
                .open(
                    &existing,
                    OpenOptions::read_write().direct_io(DirectIo::Required),
                )
                .await,
        ),
    ));
    log.push((
        "existing + create_new + Required",
        observe(
            provider
                .open(
                    &existing,
                    OpenOptions::create_new_write().direct_io(DirectIo::Required),
                )
                .await,
        ),
    ));

    let missing = format!("{prefix}missing.db");
    log.push((
        "missing without create + Required",
        observe(
            provider
                .open(
                    &missing,
                    OpenOptions::read_only().direct_io(DirectIo::Required),
                )
                .await,
        ),
    ));
    log.push((
        "missing without create + Required: exists after",
        Observation::Exists(provider.exists(&missing).await?),
    ));

    let created = format!("{prefix}missing-create.db");
    log.push((
        "missing + create + Required",
        observe(
            provider
                .open(
                    &created,
                    OpenOptions::new()
                        .write(true)
                        .create(true)
                        .direct_io(DirectIo::Required),
                )
                .await,
        ),
    ));
    log.push((
        "missing + create + Required: exists after",
        Observation::Exists(provider.exists(&created).await?),
    ));

    let exclusive = format!("{prefix}missing-create-new.db");
    log.push((
        "missing + create_new + Required",
        observe(
            provider
                .open(
                    &exclusive,
                    OpenOptions::create_new_write().direct_io(DirectIo::Required),
                )
                .await,
        ),
    ));
    log.push((
        "missing + create_new + Required: exists after",
        Observation::Exists(provider.exists(&exclusive).await?),
    ));

    Ok(log)
}

fn observation_of(log: &Log, label: &str) -> Observation {
    log.iter().find(|(name, _)| *name == label).map_or_else(
        || panic!("no step labelled {label:?}"),
        |(_, observation)| *observation,
    )
}

type Log = Vec<(&'static str, Observation)>;

/// The invariant, in the form that holds everywhere: the simulator allowed
/// exactly what production allowed. Nothing opened that production refused,
/// and nothing exists that production did not create.
fn assert_same_permissiveness(production: &Log, simulated: &Log) {
    let allowed = |log: &Log| {
        log.iter()
            .map(|(label, observation)| (*label, permitted(*observation)))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        allowed(production),
        allowed(simulated),
        "the simulated provider must allow exactly what production allows"
    );
}

/// Equally platform-independent, and what stops both backends from being wrong
/// in the same way: a refused open creates nothing, an existing file is never
/// taken by `create_new`, and `Required` never hands back a buffered file.
fn assert_structural_claims(backend: &str, log: &Log) {
    for label in [
        "missing without create + Required: exists after",
        "missing + create + Required: exists after",
        "missing + create_new + Required: exists after",
    ] {
        assert_eq!(
            observation_of(log, label),
            Observation::Exists(false),
            "{backend}: a refused Required open must create nothing, at {label:?}"
        );
    }
    assert!(
        !matches!(
            observation_of(log, "existing + create_new + Required"),
            Observation::Opened { .. }
        ),
        "{backend}: create_new must not succeed against a file that exists"
    );
    assert!(
        log.iter()
            .all(|(_, observation)| !matches!(observation, Observation::Opened { direct: false })),
        "{backend}: Required must never hand back a buffered file"
    );
}

/// Nothing here can be opened, so every scenario collapses onto the one
/// refusal — and both backends must collapse the same way.
fn assert_without_direct_io(backend: &str, log: &Log) {
    for (label, observation) in log {
        if matches!(observation, Observation::Exists(_)) {
            continue;
        }
        assert_eq!(
            *observation,
            Observation::Refused(std::io::ErrorKind::Unsupported),
            "{backend}: without direct I/O every Required open is Unsupported, at {label:?}"
        );
    }
}

/// With direct I/O the contract is fully discriminating, and these are its own
/// error kinds.
fn assert_with_direct_io(backend: &str, log: &Log) {
    for (label, expected) in [
        (
            "missing without create + Required",
            Observation::Refused(std::io::ErrorKind::NotFound),
        ),
        (
            "missing + create + Required",
            Observation::Refused(std::io::ErrorKind::Unsupported),
        ),
        (
            "missing + create_new + Required",
            Observation::Refused(std::io::ErrorKind::Unsupported),
        ),
        (
            "existing + create_new + Required",
            Observation::Refused(std::io::ErrorKind::AlreadyExists),
        ),
    ] {
        assert_eq!(
            observation_of(log, label),
            expected,
            "{backend} diverged from the contract at {label:?}"
        );
    }
}

/// The same five scenarios against both providers, which must not diverge.
///
/// What is asserted is layered, because only some of it is platform-
/// independent. Always: the simulator allowed exactly what production allowed,
/// and nothing that must be refused created a file. Where the platform has
/// direct I/O, the contract is fully discriminating and the two backends must
/// agree down to the error kind; where it has none — macOS — every `Required`
/// open is `Unsupported` on both, and the create contract is out of reach
/// there because there is no capability to secure.
#[test]
fn the_simulator_answers_the_required_contract_exactly_as_production_does() {
    local_runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let prefix = format!("{}/", dir.path().to_str().expect("temp path is UTF-8"));
        let production = required_contract(TokioStorageProvider::new(), prefix)
            .await
            .expect("the production scenarios must run");

        // Whether this build and this filesystem can do direct I/O at all.
        // macOS cannot, and refuses `Required` before it ever resolves the
        // path, which changes which refusal surfaces first.
        let direct_io_available = matches!(
            observation_of(&production, "existing + Required"),
            Observation::Opened { direct: true }
        );

        // Match the simulated disk to what production could actually do, so a
        // divergence is one of contract and not of hardware.
        let mut config = StorageConfiguration::fast_local();
        config.direct_io_supported = direct_io_available;
        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let simulated =
            run_storage_test(sim, |provider| required_contract(provider, String::new()))
                .await
                .expect("the simulated scenarios must run");

        assert_same_permissiveness(&production, &simulated);
        for (backend, log) in [("production", &production), ("simulation", &simulated)] {
            assert_structural_claims(backend, log);
            if direct_io_available {
                assert_with_direct_io(backend, log);
            } else {
                assert_without_direct_io(backend, log);
            }
        }

        if direct_io_available {
            assert_eq!(
                production, simulated,
                "the simulated provider must answer the Required contract exactly as production \
                 does"
            );
        }
    });
}
