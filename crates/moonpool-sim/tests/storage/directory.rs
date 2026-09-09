//! Directory-entry durability: `sync_dir` versus `sync_all`.
//!
//! A file sync makes the *bytes* survive a crash. It says nothing about the
//! *name* that reaches them. These tests pin that distinction — and the fault
//! that punishes an engine which forgets the second half.

use futures::io::AsyncWriteExt;
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimStorageProvider, SimWorld, StorageConfiguration};
use std::net::IpAddr;

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

/// A world whose crashes always lose an unsynced directory entry, so the
/// distinction is deterministic rather than statistical.
fn losing_sim() -> SimWorld {
    let mut config = StorageConfiguration::fast_local();
    config.unsynced_dir_entry_loss_probability = 1.0;
    let mut sim = SimWorld::new();
    sim.set_storage_config(config);
    sim
}

async fn run_on<F, Fut, T>(sim: &mut SimWorld, f: F) -> T
where
    F: FnOnce(SimStorageProvider) -> Fut,
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

/// Write and `sync_all` a brand-new file, then crash without syncing the
/// directory: the bytes were durable, the name was not.
#[test]
fn file_sync_does_not_make_the_directory_entry_durable() {
    local_runtime().block_on(async {
        let mut sim = losing_sim();

        run_on(&mut sim, |provider| async move {
            let mut file = provider
                .open("db/wal", OpenOptions::create_write())
                .await
                .expect("open failed");
            file.write_all(b"committed").await.expect("write failed");
            file.sync_all().await.expect("sync failed");
            // Deliberately no sync_dir.
        })
        .await;

        sim.simulate_crash_for_process(test_ip(), true);

        let exists = run_on(&mut sim, |provider| async move {
            provider.exists("db/wal").await.expect("exists failed")
        })
        .await;

        assert!(
            !exists,
            "an unsynced directory entry must not survive a crash"
        );
    });
}

/// The same run with a directory sync: the name survives too.
#[test]
fn a_synced_directory_entry_survives() {
    local_runtime().block_on(async {
        let mut sim = losing_sim();

        run_on(&mut sim, |provider| async move {
            let mut file = provider
                .open("db/wal", OpenOptions::create_write())
                .await
                .expect("open failed");
            file.write_all(b"committed").await.expect("write failed");
            file.sync_all().await.expect("sync failed");
            provider.sync_dir("db").await.expect("sync_dir failed");
        })
        .await;

        sim.simulate_crash_for_process(test_ip(), true);

        let exists = run_on(&mut sim, |provider| async move {
            provider.exists("db/wal").await.expect("exists failed")
        })
        .await;

        assert!(exists, "a synced directory entry must survive a crash");
    });
}

/// A delete is a directory operation too: unsynced, the crash brings the name
/// back; synced, it stays gone.
#[test]
fn delete_durability_needs_the_directory_sync() {
    local_runtime().block_on(async {
        for (sync_after_delete, expected) in [(false, true), (true, false)] {
            let mut sim = losing_sim();

            run_on(&mut sim, |provider| async move {
                let file = provider
                    .open("db/stale", OpenOptions::create_write())
                    .await
                    .expect("open failed");
                file.sync_all().await.expect("sync failed");
                drop(file);
                provider.sync_dir("db").await.expect("sync_dir failed");

                provider.delete("db/stale").await.expect("delete failed");
                if sync_after_delete {
                    provider.sync_dir("db").await.expect("sync_dir failed");
                }
            })
            .await;

            sim.simulate_crash_for_process(test_ip(), true);

            let exists = run_on(&mut sim, |provider| async move {
                provider.exists("db/stale").await.expect("exists failed")
            })
            .await;

            assert_eq!(
                exists, expected,
                "delete with sync_dir={sync_after_delete} resolved wrongly across the crash"
            );
        }
    });
}

/// Rename is the durability trick engines rely on for atomic replacement, and
/// it needs the directory sync just as much.
#[test]
fn rename_durability_needs_the_directory_sync() {
    local_runtime().block_on(async {
        let mut sim = losing_sim();

        run_on(&mut sim, |provider| async move {
            let mut file = provider
                .open("db/manifest.tmp", OpenOptions::create_write())
                .await
                .expect("open failed");
            file.write_all(b"v2").await.expect("write failed");
            file.sync_all().await.expect("sync failed");
            drop(file);
            provider.sync_dir("db").await.expect("sync_dir failed");

            provider
                .rename("db/manifest.tmp", "db/manifest")
                .await
                .expect("rename failed");
            // No sync_dir after the rename.
        })
        .await;

        sim.simulate_crash_for_process(test_ip(), true);

        let (old, new) = run_on(&mut sim, |provider| async move {
            (
                provider.exists("db/manifest.tmp").await.expect("exists"),
                provider.exists("db/manifest").await.expect("exists"),
            )
        })
        .await;

        assert!(old, "the unsynced rename must roll back to the old name");
        assert!(!new, "the unsynced rename must not be durable");
    });
}

/// A directory sync promotes the entries of *that* directory only.
#[test]
fn sync_dir_is_scoped_to_one_directory() {
    local_runtime().block_on(async {
        let mut sim = losing_sim();

        run_on(&mut sim, |provider| async move {
            for path in ["a/one", "b/two"] {
                let file = provider
                    .open(path, OpenOptions::create_write())
                    .await
                    .expect("open failed");
                file.sync_all().await.expect("sync failed");
            }
            provider.sync_dir("a").await.expect("sync_dir failed");
        })
        .await;

        sim.simulate_crash_for_process(test_ip(), true);

        let (a, b) = run_on(&mut sim, |provider| async move {
            (
                provider.exists("a/one").await.expect("exists"),
                provider.exists("b/two").await.expect("exists"),
            )
        })
        .await;

        assert!(a, "the synced directory keeps its entry");
        assert!(!b, "the unsynced directory does not");
    });
}

/// With the family off — the default — a crash keeps the namespace it had, so
/// tests that do not care about entry durability are not made to care.
#[test]
fn namespace_survives_when_the_family_is_off() {
    local_runtime().block_on(async {
        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration::fast_local());

        run_on(&mut sim, |provider| async move {
            let file = provider
                .open("db/wal", OpenOptions::create_write())
                .await
                .expect("open failed");
            file.sync_all().await.expect("sync failed");
        })
        .await;

        sim.simulate_crash_for_process(test_ip(), true);

        let exists = run_on(&mut sim, |provider| async move {
            provider.exists("db/wal").await.expect("exists failed")
        })
        .await;

        assert!(exists);
    });
}

/// One process's crash resolves one process's namespace. A neighbour's
/// unsynced directory entry is not made durable by someone else crashing, and
/// is still at risk when that neighbour crashes in turn.
#[test]
fn a_crash_resolves_only_the_crashing_processs_namespace() {
    local_runtime().block_on(async {
        let mut sim = losing_sim();
        let neighbour: IpAddr = "127.0.0.2".parse().expect("valid IP");

        // Both processes create a file; neither syncs its directory.
        for owner in [test_ip(), neighbour] {
            let provider = sim.storage_provider(owner);
            let path = format!("db/{owner}");
            let handle = tokio::spawn(async move {
                let file = provider
                    .open(&path, OpenOptions::create_write())
                    .await
                    .expect("open failed");
                file.sync_all().await.expect("sync failed");
            });
            while !handle.is_finished() {
                while sim.pending_event_count() > 0 {
                    sim.step();
                }
                tokio::task::yield_now().await;
            }
            handle.await.expect("task panicked");
        }

        // Crash only the first process.
        sim.simulate_crash_for_process(test_ip(), true);

        let exists = |sim: &mut SimWorld, owner: IpAddr, path: String| {
            let provider = sim.storage_provider(owner);
            async move { provider.exists(&path).await.expect("exists failed") }
        };

        let neighbour_path = format!("db/{neighbour}");
        let mine = run_on(&mut sim, {
            let path = format!("db/{}", test_ip());
            move |provider| async move { provider.exists(&path).await.expect("exists failed") }
        })
        .await;
        assert!(!mine, "the crashing process loses its unsynced entry");

        // The neighbour's entry is untouched by someone else's crash...
        let still_there = exists(&mut sim, neighbour, neighbour_path.clone()).await;
        assert!(still_there, "a neighbour's namespace is not resolved");

        // ...and is still unsynced, so its own crash still loses it.
        sim.simulate_crash_for_process(neighbour, true);
        let after = exists(&mut sim, neighbour, neighbour_path).await;
        assert!(
            !after,
            "the neighbour's entry was never made durable by the other crash"
        );
    });
}
