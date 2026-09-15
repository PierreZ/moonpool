//! Directory-entry durability: `sync_dir` versus `sync_all`.
//!
//! A file sync makes the *bytes* survive a crash. It says nothing about the
//! *name* that reaches them. These tests pin that distinction — and the fault
//! that punishes an engine which forgets the second half.

use futures::io::AsyncWriteExt;
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{
    CrashOutcome, EioTarget, SimStorageProvider, SimWorld, StorageConfiguration, executor::Executor,
};
use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;
use std::task::Poll;

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

/// Drive a low-level provider future and its scheduled storage events on
/// Moonpool's executor, without a Tokio runtime inside the simulation.
fn drive_on<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    let mut executor = Executor::new(7);
    executor.block_on(async {
        futures::pin_mut!(future);
        futures::future::poll_fn(|cx| match future.as_mut().poll(cx) {
            Poll::Ready(output) => Poll::Ready(output),
            Poll::Pending if sim.has_pending_events() => {
                sim.step();
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Pending => Poll::Pending,
        })
        .await
    })
}

/// A crash profile that makes the fault-coordinate path observable: the first
/// dirty sector is lost only when its surviving name is admitted by the mask.
fn masked_rename_sim(seed: u64, dir_loss_probability: f64) -> SimWorld {
    let mut config = StorageConfiguration::fast_local();
    config.unsynced_dir_entry_loss_probability = dir_loss_probability;
    config.clean_crash_probability = 0.0;
    config.correlated_rollback_probability = 0.0;
    config.crash_lost_probability = 1.0;
    config.length_survives_crash_probability = 1.0;
    let mut sim = SimWorld::new_with_seed(seed);
    sim.set_storage_config(config);
    sim.set_storage_eligibility_mask(Arc::new(|path, sector| path == "a-original" && sector == 0));
    sim
}

fn make_unsynced_rename(sim: &mut SimWorld) {
    let provider = sim.storage_provider(test_ip());
    drive_on(sim, async move {
        let file = provider
            .open("a-original", OpenOptions::create_write())
            .await
            .expect("create old name");
        file.write_at(0, b"dirty").await.expect("write old file");
        provider.sync_dir(".").await.expect("durable old name");
        provider
            .rename("a-original", "z-renamed")
            .await
            .expect("unsynced rename");
    });
}

/// Write and `sync_all` a brand-new file, then crash without syncing the
/// directory: the bytes were durable, the name was not.
#[test]
fn file_sync_does_not_make_the_directory_entry_durable() {
    local_runtime().block_on(async {
        let mut sim = losing_sim();

        run_on(&mut sim, |provider| async move {
            provider.create_dir_all("db").await.expect("create db");
            provider.sync_dir(".").await.expect("durable db");
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
            provider.create_dir_all("db").await.expect("create db");
            provider.sync_dir(".").await.expect("durable db");
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

/// Syncing entries inside a new directory does not sync the new directory's
/// own name in its parent. A crash can lose the directory and its files.
#[test]
fn syncing_new_directory_without_parent_does_not_save_its_file() {
    let mut sim = losing_sim();
    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        provider.create_dir_all("db").await.expect("create db");
        let file = provider
            .open("db/wal", OpenOptions::create_write())
            .await
            .expect("create wal");
        file.write_at(0, b"committed").await.expect("write wal");
        file.sync_all().await.expect("sync bytes");
        provider.sync_dir("db").await.expect("sync wal name");
        // No sync_dir("."): the new db name is still unsynced.
    });
    sim.simulate_crash_for_process(test_ip(), true);

    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        assert!(!provider.exists("db").await.expect("db exists"));
        assert!(!provider.exists("db/wal").await.expect("wal exists"));
    });
    assert_eq!(sim.take_storage_crash_reports().len(), 1);
}

/// Even a durable child entry cannot outlive an unsynced parent name.
#[test]
fn durable_nested_child_is_pruned_when_parent_directory_is_lost() {
    let mut sim = losing_sim();
    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        provider
            .create_dir_all("db/nested")
            .await
            .expect("create dirs");
        provider.sync_dir("db").await.expect("sync nested name");
        let file = provider
            .open("db/nested/wal", OpenOptions::create_write())
            .await
            .expect("create wal");
        file.sync_all().await.expect("sync bytes");
        provider.sync_dir("db/nested").await.expect("sync wal name");
    });
    sim.simulate_crash_for_process(test_ip(), true);

    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        for path in ["db", "db/nested", "db/nested/wal"] {
            assert!(
                !provider.exists(path).await.expect("exists"),
                "{path} survived"
            );
        }
    });
}

#[test]
fn syncing_each_parent_makes_nested_directory_and_file_survive() {
    let mut sim = losing_sim();
    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        provider
            .create_dir_all("db/nested")
            .await
            .expect("create dirs");
        provider.sync_dir(".").await.expect("sync db name");
        provider.sync_dir("db").await.expect("sync nested name");
        let file = provider
            .open("db/nested/wal", OpenOptions::create_write())
            .await
            .expect("create wal");
        file.sync_all().await.expect("sync bytes");
        provider.sync_dir("db/nested").await.expect("sync wal name");
    });
    sim.simulate_crash_for_process(test_ip(), true);

    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        for path in ["db", "db/nested", "db/nested/wal"] {
            assert!(
                provider.exists(path).await.expect("exists"),
                "{path} was lost"
            );
        }
    });
}

#[test]
fn targeted_faults_resolve_directory_aliases() {
    let mut sim = SimWorld::new();
    sim.set_storage_config(StorageConfiguration::fast_local());
    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        provider.create_dir_all("db").await.expect("create db");
        let file = provider
            .open("db/wal", OpenOptions::create_write())
            .await
            .expect("create wal");
        file.write_at(0, b"seed").await.expect("seed wal");
    });
    sim.fail_file_with_eio("./db/../db/wal", 0..1, EioTarget::Write)
        .expect("alias targets wal");
    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        let file = provider
            .open("./db/wal", OpenOptions::read_write())
            .await
            .expect("open alias");
        assert!(file.write_at(0, b"changed").await.is_err());
    });
}

#[test]
fn relative_and_absolute_roots_have_distinct_namespaces() {
    let mut sim = SimWorld::new();
    sim.set_storage_config(StorageConfiguration::fast_local());
    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        provider.create_dir_all("db").await.expect("relative db");
        provider.create_dir_all("/db").await.expect("absolute db");
        let relative = provider
            .open("db/wal", OpenOptions::create_new_write())
            .await
            .expect("relative wal");
        let absolute = provider
            .open("/db/wal", OpenOptions::create_new_write())
            .await
            .expect("absolute wal");
        relative
            .write_at(0, b"relative")
            .await
            .expect("relative bytes");
        absolute
            .write_at(0, b"absolute")
            .await
            .expect("absolute bytes");
        assert_eq!(relative.size().await.expect("relative size"), 8);
        assert_eq!(absolute.size().await.expect("absolute size"), 8);
    });
}

#[test]
fn directory_rename_is_rejected_without_moving_its_children() {
    let mut sim = SimWorld::new();
    sim.set_storage_config(StorageConfiguration::fast_local());
    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        provider.create_dir_all("db").await.expect("create db");
        let file = provider
            .open("db/wal", OpenOptions::create_write())
            .await
            .expect("create wal");
        drop(file);
        assert!(provider.rename("db", "renamed").await.is_err());
        assert!(provider.exists("db/wal").await.expect("wal exists"));
        assert!(!provider.exists("renamed").await.expect("renamed exists"));
    });
}

/// Replacing a durable file name with an unsynced directory must choose one
/// complete entry type per crash, never an invisible file behind a directory.
#[test]
fn file_to_directory_replacement_crash_keeps_one_entry_type() {
    let mut seen_file = false;
    let mut seen_directory = false;
    for seed in 0..64 {
        let mut config = StorageConfiguration::fast_local();
        config.unsynced_dir_entry_loss_probability = 0.5;
        let mut sim = SimWorld::new_with_seed(seed);
        sim.set_storage_config(config);
        let provider = sim.storage_provider(test_ip());
        drive_on(&mut sim, async move {
            let file = provider
                .open("x", OpenOptions::create_write())
                .await
                .expect("create x file");
            file.write_at(0, b"file").await.expect("write x file");
            file.sync_all().await.expect("sync x bytes");
            provider.sync_dir(".").await.expect("sync x name");
            drop(file);
            provider.delete("x").await.expect("unlink x file");
            provider
                .create_dir_all("x")
                .await
                .expect("replace with x dir");
        });
        sim.simulate_crash_for_process(test_ip(), true);

        let provider = sim.storage_provider(test_ip());
        let (file, directory) = drive_on(&mut sim, async move {
            (
                provider.open("x", OpenOptions::read_only()).await.is_ok(),
                provider.sync_dir("x").await.is_ok(),
            )
        });
        assert_ne!(file, directory, "seed {seed} left conflicting entry types");
        seen_file |= file;
        seen_directory |= directory;
    }
    assert!(
        seen_file && seen_directory,
        "replacement sweep missed an outcome"
    );
}

/// A delete is a directory operation too: unsynced, the crash brings the name
/// back; synced, it stays gone.
#[test]
fn delete_durability_needs_the_directory_sync() {
    local_runtime().block_on(async {
        for (sync_after_delete, expected) in [(false, true), (true, false)] {
            let mut sim = losing_sim();

            run_on(&mut sim, |provider| async move {
                provider.create_dir_all("db").await.expect("create db");
                provider.sync_dir(".").await.expect("durable db");
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
            provider.create_dir_all("db").await.expect("create db");
            provider.sync_dir(".").await.expect("durable db");
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

/// When a crash rolls a rename back, the original name must govern that
/// crash's fault mask and reports, and later I/O through the recovered file.
#[test]
fn rolled_back_rename_restores_the_original_fault_coordinate() {
    let mut sim = masked_rename_sim(17, 1.0);
    make_unsynced_rename(&mut sim);
    sim.simulate_crash_for_process(test_ip(), true);

    let reports = sim.take_storage_crash_reports();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].path, "a-original");
    assert_eq!(reports[0].resolutions[0].outcome, CrashOutcome::Lost);

    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        assert!(
            provider
                .exists("a-original")
                .await
                .expect("old name exists")
        );
        assert!(!provider.exists("z-renamed").await.expect("new name absent"));
    });

    let mut config = StorageConfiguration::fast_local();
    config.write_eio_probability = 1.0;
    sim.set_storage_config(config);
    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        let file = provider
            .open("a-original", OpenOptions::read_write())
            .await
            .expect("reopen recovered file");
        let error = file.write_at(0, b"later").await.expect_err("masked EIO");
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
    });
}

/// Per-name crash rolls can retain both rename aliases. They must share one
/// image and use the first surviving name as their fault coordinate.
#[test]
fn surviving_rename_aliases_use_the_first_name_for_faults() {
    let mut found = false;
    for seed in 0..200_u64 {
        let mut sim = masked_rename_sim(seed, 0.5);
        make_unsynced_rename(&mut sim);
        sim.simulate_crash_for_process(test_ip(), true);
        let provider = sim.storage_provider(test_ip());
        let both_survived = drive_on(&mut sim, async move {
            provider
                .exists("a-original")
                .await
                .expect("old name exists")
                && provider.exists("z-renamed").await.expect("new name exists")
        });
        if !both_survived {
            continue;
        }
        found = true;
        let reports = sim.take_storage_crash_reports();
        assert_eq!(reports[0].path, "a-original", "seed {seed}");
        assert_eq!(
            reports[0].resolutions[0].outcome,
            CrashOutcome::Lost,
            "seed {seed}"
        );

        let mut config = StorageConfiguration::fast_local();
        config.write_eio_probability = 1.0;
        sim.set_storage_config(config);
        let provider = sim.storage_provider(test_ip());
        drive_on(&mut sim, async move {
            for name in ["a-original", "z-renamed"] {
                let file = provider
                    .open(name, OpenOptions::read_write())
                    .await
                    .expect("reopen alias");
                file.write_at(0, b"later")
                    .await
                    .expect_err("canonical masked EIO");
            }
        });
        break;
    }
    assert!(found, "some seed must retain both rename aliases");
}

/// Losing the only unsynced name must not skip the byte-crash oracle for the
/// image that existed when the power failure began.
#[test]
fn a_lost_new_name_still_gets_a_crash_report() {
    let mut config = StorageConfiguration::fast_local();
    config.unsynced_dir_entry_loss_probability = 1.0;
    config.clean_crash_probability = 0.0;
    config.length_survives_crash_probability = 1.0;
    let mut sim = SimWorld::new_with_seed(23);
    sim.set_storage_config(config);

    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        let file = provider
            .open("new-file", OpenOptions::create_write())
            .await
            .expect("create unsynced name");
        file.write_at(0, b"dirty")
            .await
            .expect("write unsynced bytes");
    });
    sim.simulate_crash_for_process(test_ip(), true);

    let reports = sim.take_storage_crash_reports();
    assert_eq!(
        reports.len(),
        1,
        "lost name must not bypass the crash oracle"
    );
    assert_eq!(reports[0].path, "new-file");
    let provider = sim.storage_provider(test_ip());
    drive_on(&mut sim, async move {
        assert!(!provider.exists("new-file").await.expect("name was lost"));
    });
}

/// A directory sync promotes the entries of *that* directory only.
#[test]
fn sync_dir_is_scoped_to_one_directory() {
    local_runtime().block_on(async {
        let mut sim = losing_sim();

        run_on(&mut sim, |provider| async move {
            for directory in ["a", "b"] {
                provider
                    .create_dir_all(directory)
                    .await
                    .expect("create dir");
            }
            provider.sync_dir(".").await.expect("durable dirs");
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
            provider.create_dir_all("db").await.expect("create db");
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
                provider.create_dir_all("db").await.expect("create db");
                provider.sync_dir(".").await.expect("durable db");
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
