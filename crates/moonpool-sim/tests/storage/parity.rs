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
) -> std::io::Result<Vec<(&'static str, Observation)>> {
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

fn observation_of(log: &[(&'static str, Observation)], label: &str) -> Observation {
    log.iter().find(|(name, _)| *name == label).map_or_else(
        || panic!("no step labelled {label:?}"),
        |(_, observation)| *observation,
    )
}

/// The same five scenarios against both providers, which must answer alike.
///
/// The equality assertion is the invariant; the named expectations below it
/// are what stops both backends being wrong in the same way.
#[test]
fn the_simulator_answers_the_required_contract_exactly_as_production_does() {
    local_runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let prefix = format!("{}/", dir.path().to_str().expect("temp path is UTF-8"));
        let production = required_contract(TokioStorageProvider::new(), prefix)
            .await
            .expect("the production scenarios must run");

        // Match the simulated disk to what this filesystem can actually do, so
        // a mismatch is a divergence of contract and not one of hardware. The
        // scenarios that matter — the ones that must be refused — are refused
        // either way.
        let mut config = StorageConfiguration::fast_local();
        config.direct_io_supported = matches!(
            observation_of(&production, "existing + Required"),
            Observation::Opened { direct: true }
        );
        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let simulated =
            run_storage_test(sim, |provider| required_contract(provider, String::new()))
                .await
                .expect("the simulated scenarios must run");

        assert_eq!(
            production, simulated,
            "the simulated provider must answer the Required contract exactly as production does"
        );

        for (label, expected) in [
            (
                "missing without create + Required",
                Observation::Refused(std::io::ErrorKind::NotFound),
            ),
            (
                "missing without create + Required: exists after",
                Observation::Exists(false),
            ),
            (
                "missing + create + Required",
                Observation::Refused(std::io::ErrorKind::Unsupported),
            ),
            (
                "missing + create + Required: exists after",
                Observation::Exists(false),
            ),
            (
                "missing + create_new + Required",
                Observation::Refused(std::io::ErrorKind::Unsupported),
            ),
            (
                "missing + create_new + Required: exists after",
                Observation::Exists(false),
            ),
        ] {
            for (backend, log) in [("production", &production), ("simulation", &simulated)] {
                assert_eq!(
                    observation_of(log, label),
                    expected,
                    "{backend} diverged from the contract at {label:?}"
                );
            }
        }

        // Two claims that hold whatever this filesystem supports, and that no
        // amount of agreement between the backends can excuse.
        for (backend, log) in [("production", &production), ("simulation", &simulated)] {
            assert!(
                !matches!(
                    observation_of(log, "existing + create_new + Required"),
                    Observation::Opened { .. }
                ),
                "{backend}: create_new must not succeed against a file that exists"
            );
            assert!(
                log.iter().all(|(_, observation)| !matches!(
                    observation,
                    Observation::Opened { direct: false }
                )),
                "{backend}: Required must never hand back a buffered file"
            );
        }
    });
}
