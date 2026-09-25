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
//!
//! The flag rules are the second such gap: `std` refuses a contradictory
//! `OpenOptions` (a read-only `truncate`, a `create` without write access)
//! before the operating system sees it, and the simulator used to honor the
//! truncation on the way to a successful open.

use crate::{local_runtime, run_storage_test};
use futures::io::{AsyncSeekExt, AsyncWriteExt};
use moonpool_core::{DirectIo, OpenOptions, StorageFile, StorageProvider, TokioStorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration};
use std::io::SeekFrom;
use tempfile::TempDir;

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

/// One entry of the flag contract: what the open answered, and what the file
/// held afterwards, so a refusal that nonetheless truncated is caught.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FlagStep {
    label: &'static str,
    observation: Observation,
    size_after: u64,
}

const SEED_BYTES: &[u8] = b"twelve bytes";

/// Every `OpenOptions` shape `std` refuses as contradictory, plus a legal
/// control at each end, run against a file whose bytes must survive the
/// refusals.
async fn flag_contract<P: StorageProvider>(
    provider: P,
    prefix: String,
) -> std::io::Result<Vec<FlagStep>> {
    let path = format!("{prefix}flags.db");
    let mut seed = provider
        .open(&path, OpenOptions::create_new_write())
        .await?;
    seed.write_all(SEED_BYTES).await?;
    seed.sync_all().await?;
    drop(seed);

    let shapes: [(&'static str, OpenOptions); 7] = [
        ("read-only", OpenOptions::read_only()),
        (
            "read-only + truncate",
            OpenOptions::read_only().truncate(true),
        ),
        ("read-only + create", OpenOptions::read_only().create(true)),
        (
            "read-only + create_new",
            OpenOptions::read_only().create_new(true),
        ),
        ("no access mode", OpenOptions::new()),
        (
            "append + truncate",
            OpenOptions::new().append(true).truncate(true),
        ),
        ("read-write", OpenOptions::read_write()),
    ];

    let mut log = Vec::with_capacity(shapes.len());
    for (label, options) in shapes {
        let observation = observe(provider.open(&path, options).await);
        let probe = provider.open(&path, OpenOptions::read_only()).await?;
        let size_after = probe.size().await?;
        drop(probe);
        log.push(FlagStep {
            label,
            observation,
            size_after,
        });
    }
    Ok(log)
}

/// The flag rules are `std`'s and platform-independent, so here the two
/// backends must agree exactly: the same shapes refused, with the same error
/// kind, and the seeded bytes intact after every one of them.
#[test]
fn the_simulator_refuses_the_open_flags_production_refuses() {
    local_runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let prefix = format!("{}/", dir.path().to_str().expect("temp path is UTF-8"));
        let production = flag_contract(TokioStorageProvider::new(), prefix)
            .await
            .expect("the production scenarios must run");

        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration::fast_local());
        let simulated = run_storage_test(sim, |provider| flag_contract(provider, String::new()))
            .await
            .expect("the simulated scenarios must run");

        assert_eq!(
            production, simulated,
            "the simulated provider must refuse exactly the open flags production refuses"
        );
        for step in &production {
            assert_eq!(
                step.size_after,
                SEED_BYTES.len() as u64,
                "a refused open must not truncate, at {:?}",
                step.label
            );
            let legal = matches!(step.label, "read-only" | "read-write");
            assert_eq!(
                permitted(step.observation),
                legal,
                "unexpected verdict at {:?}: {:?}",
                step.label,
                step.observation
            );
            if !legal {
                assert_eq!(
                    step.observation,
                    Observation::Refused(std::io::ErrorKind::InvalidInput),
                    "contradictory flags are InvalidInput, at {:?}",
                    step.label
                );
            }
        }
    });
}

/// The result of a namespace operation, ignoring platform-specific text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathOutcome {
    Applied(Result<(), std::io::ErrorKind>),
    Exists(Result<bool, std::io::ErrorKind>),
}

fn applied<T>(result: std::io::Result<T>) -> PathOutcome {
    PathOutcome::Applied(result.map(|_| ()).map_err(|error| error.kind()))
}

fn existence(result: std::io::Result<bool>) -> PathOutcome {
    PathOutcome::Exists(result.map_err(|error| error.kind()))
}

/// Resolve the same parent, alias, collision, and empty-path cases on both
/// backends. Every rejection is followed by an existence or size probe so a
/// refused operation that mutated the namespace is visible.
async fn path_contract<P: StorageProvider>(
    provider: P,
    prefix: String,
) -> std::io::Result<Vec<(&'static str, PathOutcome)>> {
    let path = |name: &str| format!("{prefix}{name}");
    let mut log = Vec::new();
    let file = provider
        .open(&path("seed"), OpenOptions::create_new_write())
        .await?;
    file.write_at(0, SEED_BYTES).await?;
    drop(file);

    alias_and_missing_parent_contract(&provider, &prefix, &mut log).await;
    directory_contract(&provider, &prefix, &mut log).await?;
    file_as_parent_contract(&provider, &prefix, &mut log).await?;
    invalid_name_contract(&provider, &prefix, &mut log).await;
    Ok(log)
}

/// Dot aliases and paths under a missing parent.
async fn alias_and_missing_parent_contract<P: StorageProvider>(
    provider: &P,
    prefix: &str,
    log: &mut Vec<(&'static str, PathOutcome)>,
) {
    let path = |name: &str| format!("{prefix}{name}");
    log.push((
        "dot alias open",
        applied(
            provider
                .open(&path("./seed"), OpenOptions::read_only())
                .await,
        ),
    ));
    log.push((
        "dot alias exists",
        existence(provider.exists(&path("./seed")).await),
    ));
    log.push((
        "missing parent open",
        applied(
            provider
                .open(&path("missing/child"), OpenOptions::create_write())
                .await,
        ),
    ));
    log.push((
        "missing parent still absent",
        existence(provider.exists(&path("missing")).await),
    ));
    log.push((
        "missing parent sync",
        applied(provider.sync_dir(&path("missing")).await),
    ));
    log.push((
        "missing before dotdot",
        applied(
            provider
                .open(&path("missing/../seed"), OpenOptions::read_only())
                .await,
        ),
    ));
    log.push((
        "missing before dotdot exists",
        existence(provider.exists(&path("missing/../seed")).await),
    ));
}

/// Directory creation, aliasing, and the file-only operations a directory refuses.
async fn directory_contract<P: StorageProvider>(
    provider: &P,
    prefix: &str,
    log: &mut Vec<(&'static str, PathOutcome)>,
) -> std::io::Result<()> {
    let path = |name: &str| format!("{prefix}{name}");
    log.push((
        "mkdir nested",
        applied(provider.create_dir_all(&path("db/nested")).await),
    ));
    log.push((
        "mkdir idempotent",
        applied(provider.create_dir_all(&path("db/nested")).await),
    ));
    log.push((
        "directory exists",
        existence(provider.exists(&path("db")).await),
    ));
    log.push((
        "nested directory exists",
        existence(provider.exists(&path("db/nested")).await),
    ));
    log.push((
        "dot directory sync",
        applied(provider.sync_dir(&path("./db")).await),
    ));
    log.push((
        "rename dot alias",
        applied(provider.rename(&path("./seed"), &path("renamed")).await),
    ));
    log.push((
        "rename back dot alias",
        applied(provider.rename(&path("./renamed"), &path("seed")).await),
    ));
    log.push((
        "directory before dotdot alias",
        applied(
            provider
                .open(&path("db/../seed"), OpenOptions::read_only())
                .await,
        ),
    ));
    log.push((
        "open directory exclusive",
        applied(
            provider
                .open(&path("db"), OpenOptions::create_new_write())
                .await,
        ),
    ));
    let delete_error = provider
        .delete(&path("db"))
        .await
        .expect_err("directory deletion must fail");
    // macOS reports PermissionDenied, while Linux reports IsADirectory.
    // Both reject this file-only operation.
    assert!(
        matches!(
            delete_error.kind(),
            std::io::ErrorKind::IsADirectory | std::io::ErrorKind::PermissionDenied
        ),
        "unexpected directory deletion error: {delete_error}"
    );
    log.push((
        "delete directory",
        PathOutcome::Applied(Err(std::io::ErrorKind::IsADirectory)),
    ));
    assert!(provider.exists(&path("db")).await?, "directory survived");
    assert!(
        provider.exists(&path("db/nested")).await?,
        "nested directory survived"
    );
    Ok(())
}

/// A regular file where a directory is expected.
async fn file_as_parent_contract<P: StorageProvider>(
    provider: &P,
    prefix: &str,
    log: &mut Vec<(&'static str, PathOutcome)>,
) -> std::io::Result<()> {
    let path = |name: &str| format!("{prefix}{name}");
    let regular = provider
        .open(&path("regular"), OpenOptions::create_new_write())
        .await?;
    drop(regular);
    log.push((
        "sync regular file",
        applied(provider.sync_dir(&path("regular")).await),
    ));
    let trash = provider
        .open(&path("trash"), OpenOptions::create_new_write())
        .await?;
    drop(trash);
    log.push((
        "delete dot alias",
        applied(provider.delete(&path("./trash")).await),
    ));
    log.push((
        "deleted alias absent",
        existence(provider.exists(&path("trash")).await),
    ));
    log.push((
        "file as parent open",
        applied(
            provider
                .open(&path("regular/child"), OpenOptions::create_write())
                .await,
        ),
    ));
    log.push((
        "file as parent exists",
        existence(provider.exists(&path("regular/child")).await),
    ));
    log.push((
        "file as parent mkdir",
        applied(provider.create_dir_all(&path("regular/child")).await),
    ));
    log.push((
        "file before dotdot",
        applied(
            provider
                .open(&path("regular/../seed"), OpenOptions::read_only())
                .await,
        ),
    ));
    Ok(())
}

/// Empty and NUL names, then the seed file still in place.
async fn invalid_name_contract<P: StorageProvider>(
    provider: &P,
    prefix: &str,
    log: &mut Vec<(&'static str, PathOutcome)>,
) {
    let path = |name: &str| format!("{prefix}{name}");
    log.push((
        "empty open",
        applied(provider.open("", OpenOptions::create_write()).await),
    ));
    log.push(("empty sync", applied(provider.sync_dir("").await)));
    log.push(("empty mkdir", applied(provider.create_dir_all("").await)));
    log.push((
        "NUL open",
        applied(
            provider
                .open("bad\0name", OpenOptions::create_write())
                .await,
        ),
    ));
    log.push((
        "seed still exists",
        existence(provider.exists(&path("seed")).await),
    ));
}

#[test]
fn simulated_paths_match_tokio_namespace_behavior() {
    local_runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let prefix = format!("{}/", dir.path().to_str().expect("temp path is UTF-8"));
        let production = path_contract(TokioStorageProvider::new(), prefix)
            .await
            .expect("production path scenario");
        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration::fast_local());
        let simulated = run_storage_test(sim, |provider| path_contract(provider, String::new()))
            .await
            .expect("simulated path scenario");
        assert_eq!(production, simulated, "path behavior must match Tokio");
    });
}

/// What each listing answered: the names, or the kind of the refusal.
type Listing = (&'static str, Result<Vec<String>, std::io::ErrorKind>);

/// Listings of the same tree on both backends: sorted bare names, files and
/// directories alike, refusals for a missing path and for a file, and the
/// namespace changes a delete and a rename make.
async fn listing_contract<P: StorageProvider>(
    provider: P,
    root: String,
) -> std::io::Result<Vec<Listing>> {
    let path = |name: &str| format!("{root}/{name}");
    let listed = |result: std::io::Result<Vec<String>>| result.map_err(|error| error.kind());
    for name in ["b", "a", "c"] {
        drop(
            provider
                .open(&path(name), OpenOptions::create_new_write())
                .await?,
        );
    }
    provider.create_dir_all(&path("dir/nested")).await?;
    drop(
        provider
            .open(&path("dir/x"), OpenOptions::create_new_write())
            .await?,
    );

    let mut log = vec![
        ("root", listed(provider.list_dir(&root).await)),
        ("dir", listed(provider.list_dir(&path("dir")).await)),
        ("dot dir", listed(provider.list_dir(&path("./dir/")).await)),
        (
            "empty dir",
            listed(provider.list_dir(&path("dir/nested")).await),
        ),
        ("missing", listed(provider.list_dir(&path("missing")).await)),
        ("file", listed(provider.list_dir(&path("a")).await)),
    ];
    provider.delete(&path("b")).await?;
    provider.rename(&path("c"), &path("dir/y")).await?;
    log.push((
        "root after delete and rename",
        listed(provider.list_dir(&root).await),
    ));
    log.push((
        "dir after rename",
        listed(provider.list_dir(&path("dir")).await),
    ));
    Ok(log)
}

#[test]
fn simulated_listings_match_tokio() {
    local_runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let root = format!("{}/tree", dir.path().to_str().expect("temp path is UTF-8"));
        TokioStorageProvider::new()
            .create_dir_all(&root)
            .await
            .expect("create root");
        let production = listing_contract(TokioStorageProvider::new(), root)
            .await
            .expect("production listing scenario");
        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration::fast_local());
        let simulated = run_storage_test(sim, |provider| async move {
            provider.create_dir_all("tree").await?;
            listing_contract(provider, "tree".to_string()).await
        })
        .await
        .expect("simulated listing scenario");
        assert_eq!(production, simulated, "listings must match Tokio");
        let names = |at: usize| production[at].1.clone().expect("listed");
        assert_eq!(names(0), ["a", "b", "c", "dir"]);
        assert_eq!(names(6), ["a", "dir"]);
        assert_eq!(names(7), ["nested", "x", "y"]);
    });
}

/// What one seek answered, and where the cursor stood afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeekStep {
    label: &'static str,
    result: Result<u64, std::io::ErrorKind>,
    position_after: u64,
}

/// Seeks that land before byte zero, with a legal control between them,
/// against a file of [`SEED_BYTES`].
async fn seek_contract<P: StorageProvider>(
    provider: P,
    prefix: String,
) -> std::io::Result<Vec<SeekStep>> {
    let path = format!("{prefix}seek.db");
    let mut file = provider
        .open(&path, OpenOptions::create_new_write())
        .await?;
    file.write_all(SEED_BYTES).await?;
    file.sync_all().await?;
    drop(file);

    let mut file = provider.open(&path, OpenOptions::read_only()).await?;
    // Only seeks *before byte zero* are asked of both backends: how far past
    // the end a file may be positioned is the filesystem's ceiling, not the
    // contract's (ext4 refuses an offset past its maximum file size where
    // others accept it), so overflow stays a simulator-only refusal.
    let seeks: [(&'static str, SeekFrom); 5] = [
        ("start 5", SeekFrom::Start(5)),
        ("current -10 from 5", SeekFrom::Current(-10)),
        ("end -3", SeekFrom::End(-3)),
        ("end -100", SeekFrom::End(-100)),
        ("current +3 from 9", SeekFrom::Current(3)),
    ];
    let mut log = Vec::with_capacity(seeks.len());
    for (label, pos) in seeks {
        let result = file.seek(pos).await.map_err(|e| e.kind());
        let position_after = file.stream_position().await?;
        log.push(SeekStep {
            label,
            result,
            position_after,
        });
    }
    Ok(log)
}

/// `lseek` refuses a negative or overflowing result with `EINVAL` and leaves
/// the cursor alone. The simulator used to clamp the target to zero and
/// report success, so a seek arithmetic bug stayed invisible until the
/// production filesystem.
#[test]
fn the_simulator_refuses_the_seeks_production_refuses() {
    local_runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let prefix = format!("{}/", dir.path().to_str().expect("temp path is UTF-8"));
        let production = seek_contract(TokioStorageProvider::new(), prefix)
            .await
            .expect("the production scenarios must run");

        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration::fast_local());
        let simulated = run_storage_test(sim, |provider| seek_contract(provider, String::new()))
            .await
            .expect("the simulated scenarios must run");

        assert_eq!(
            production, simulated,
            "the simulated provider must refuse exactly the seeks production refuses"
        );
        for step in &production {
            match step.label {
                "current -10 from 5" | "end -100" => assert_eq!(
                    step.result,
                    Err(std::io::ErrorKind::InvalidInput),
                    "at {:?}",
                    step.label
                ),
                _ => assert_eq!(step.result, Ok(step.position_after), "at {:?}", step.label),
            }
        }
    });
}

/// One step of the open-file contract: what a read through a handle, or a
/// namespace query, answered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NameStep {
    label: &'static str,
    outcome: Result<Vec<u8>, std::io::ErrorKind>,
}

async fn read_all<F: StorageFile>(file: &F, len: usize) -> Result<Vec<u8>, std::io::ErrorKind> {
    let mut bytes = vec![0_u8; len];
    let mut read = 0;
    while read < len {
        match file.read_at(read as u64, &mut bytes[read..]).await {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(error) => return Err(error.kind()),
        }
    }
    bytes.truncate(read);
    Ok(bytes)
}

/// Unlink and rename-over with handles held open: the handles keep the old
/// image, the names answer for the new one.
async fn open_file_contract<P: StorageProvider>(
    provider: P,
    prefix: String,
) -> std::io::Result<Vec<NameStep>> {
    let mut log = Vec::new();
    let mut step = |label: &'static str, outcome: Result<Vec<u8>, std::io::ErrorKind>| {
        log.push(NameStep { label, outcome });
    };
    let exists = |present: bool| Ok(vec![u8::from(present)]);

    // unlink: the open handle outlives the name.
    let alpha = format!("{prefix}alpha.db");
    let mut writer = provider
        .open(&alpha, OpenOptions::create_new_write())
        .await?;
    writer.write_all(b"alpha-bytes").await?;
    writer.sync_all().await?;
    drop(writer);
    let held = provider.open(&alpha, OpenOptions::read_write()).await?;
    provider.delete(&alpha).await?;
    step(
        "alpha exists after unlink",
        exists(provider.exists(&alpha).await?),
    );
    step("held handle reads after unlink", read_all(&held, 64).await);
    step(
        "held handle writes after unlink",
        held.write_at(0, b"ALPHA")
            .await
            .map(|n| vec![u8::try_from(n).unwrap_or(u8::MAX)])
            .map_err(|e| e.kind()),
    );
    step("held handle reads its write", read_all(&held, 64).await);
    drop(held);
    step(
        "alpha exists once the handle is gone",
        exists(provider.exists(&alpha).await?),
    );

    // rename over: the handle on the replaced file keeps the replaced image.
    let bravo = format!("{prefix}bravo.db");
    let charlie = format!("{prefix}charlie.db");
    let mut writer = provider
        .open(&bravo, OpenOptions::create_new_write())
        .await?;
    writer.write_all(b"bravo-bytes").await?;
    writer.sync_all().await?;
    drop(writer);
    let mut writer = provider
        .open(&charlie, OpenOptions::create_new_write())
        .await?;
    writer.write_all(b"charlie").await?;
    writer.sync_all().await?;
    drop(writer);
    let old = provider.open(&bravo, OpenOptions::read_only()).await?;
    provider.rename(&charlie, &bravo).await?;
    step(
        "charlie exists after rename",
        exists(provider.exists(&charlie).await?),
    );
    step(
        "old handle reads the replaced image",
        read_all(&old, 64).await,
    );
    let new = provider.open(&bravo, OpenOptions::read_only()).await?;
    step("the name reads the new image", read_all(&new, 64).await);
    drop(old);
    drop(new);
    step("bravo still exists", exists(provider.exists(&bravo).await?));
    Ok(log)
}

/// `unlink(2)` and `rename(2)` are namespace operations and never reach an
/// open file; the simulator used to invalidate every handle on the way.
#[test]
fn the_simulator_keeps_open_files_alive_across_unlink_and_rename() {
    local_runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let prefix = format!("{}/", dir.path().to_str().expect("temp path is UTF-8"));
        let production = open_file_contract(TokioStorageProvider::new(), prefix)
            .await
            .expect("the production scenarios must run");

        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration::fast_local());
        let simulated =
            run_storage_test(sim, |provider| open_file_contract(provider, String::new()))
                .await
                .expect("the simulated scenarios must run");

        assert_eq!(
            production, simulated,
            "an open file must outlive its name on both backends alike"
        );
        let outcome = |label: &str| {
            production
                .iter()
                .find(|step| step.label == label)
                .map_or_else(
                    || panic!("no step labelled {label:?}"),
                    |step| step.outcome.clone(),
                )
        };
        assert_eq!(
            outcome("held handle reads after unlink"),
            Ok(b"alpha-bytes".to_vec())
        );
        assert_eq!(
            outcome("held handle reads its write"),
            Ok(b"ALPHA-bytes".to_vec())
        );
        assert_eq!(
            outcome("old handle reads the replaced image"),
            Ok(b"bravo-bytes".to_vec())
        );
        assert_eq!(
            outcome("the name reads the new image"),
            Ok(b"charlie".to_vec())
        );
        assert_eq!(outcome("alpha exists after unlink"), Ok(vec![0]));
        assert_eq!(outcome("charlie exists after rename"), Ok(vec![0]));
    });
}
