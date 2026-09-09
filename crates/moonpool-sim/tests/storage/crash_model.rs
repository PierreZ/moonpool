//! The barrier-bounded crash model, on ordinary files.
//!
//! A sync is the only barrier. Everything written since the last one may, on a
//! crash, land old, land new, vanish, or come back damaged — independently per
//! sector. These tests pin each shape the model is allowed to produce, the two
//! outcomes it must *never* produce (losing a synced write silently, healing a
//! damaged sector on retry), and the oracle that catches the first of those.

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{
    CrashOutcome, EioTarget, SECTOR_SIZE, SimStorageProvider, SimWorld, StorageConfiguration,
    StorageFaultKind,
};
use std::net::IpAddr;
use std::sync::Arc;

const SECTOR: u64 = SECTOR_SIZE as u64;

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

/// A seeded world with a fast disk and whatever fault profile the test needs.
fn sim_with(seed: u64, config: StorageConfiguration) -> SimWorld {
    let mut sim = SimWorld::new_with_seed(seed);
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

fn sectors(count: usize, byte: u8) -> Vec<u8> {
    vec![byte; count * SECTOR_SIZE]
}

/// Read the whole file back through positioned reads.
async fn read_image(provider: &SimStorageProvider, path: &str, len: usize) -> Vec<u8> {
    let file = provider
        .open(path, OpenOptions::read_only())
        .await
        .expect("open failed");
    let mut buf = vec![0u8; len];
    let mut read = 0;
    while read < len {
        let n = file
            .read_at(read as u64, &mut buf[read..])
            .await
            .expect("read failed");
        if n == 0 {
            buf.truncate(read);
            break;
        }
        read += n;
    }
    buf
}

/// A synced write is durable and an unsynced one is not: across seeds the
/// synced prefix always survives intact, and the unsynced tail is sometimes
/// rolled back.
#[test]
fn synced_writes_survive_and_unsynced_ones_may_not() {
    let mut saw_rollback = false;
    let mut saw_survival = false;

    for seed in 0..200_u64 {
        local_runtime().block_on(async {
            let mut sim = sim_with(seed, StorageConfiguration::fast_local());

            run_on(&mut sim, |provider| async move {
                let file = provider
                    .open("wal", OpenOptions::create_write().read(true))
                    .await
                    .expect("open failed");
                file.write_at(0, &sectors(1, 0xA1)).await.expect("write");
                file.sync_all().await.expect("sync");
                // Past the barrier: this one is not durable.
                file.write_at(SECTOR, &sectors(1, 0xB2))
                    .await
                    .expect("write");
            })
            .await;

            sim.simulate_crash_for_process(test_ip(), true);

            let image = run_on(&mut sim, |provider| async move {
                read_image(&provider, "wal", 2 * SECTOR_SIZE).await
            })
            .await;

            assert!(
                image[..SECTOR_SIZE].iter().all(|byte| *byte == 0xA1),
                "seed {seed}: a synced write must survive a crash intact"
            );
            if image.len() > SECTOR_SIZE {
                if image[SECTOR_SIZE..].iter().all(|byte| *byte == 0xB2) {
                    saw_survival = true;
                } else {
                    saw_rollback = true;
                }
            } else {
                saw_rollback = true;
            }
        });

        if saw_rollback && saw_survival {
            break;
        }
    }

    assert!(saw_rollback, "an unsynced write must sometimes be lost");
    assert!(
        saw_survival,
        "an unsynced write must sometimes survive — a crash is not a truncation"
    );
}

/// The shape journal recovery exists to survive: a later unsynced write lands
/// intact while an earlier one is torn across its sectors.
#[test]
fn a_crash_reorders_and_tears_unsynced_writes() {
    let mut found = false;

    for seed in 0..400_u64 {
        let torn = local_runtime().block_on(async {
            let mut sim = sim_with(seed, StorageConfiguration::fast_local());

            run_on(&mut sim, |provider| async move {
                let file = provider
                    .open("journal", OpenOptions::create_write().read(true))
                    .await
                    .expect("open failed");
                file.set_len(8 * SECTOR).await.expect("set_len");
                file.sync_all().await.expect("sync");
                // Two unsynced writes, no barrier between them.
                file.write_at(0, &sectors(4, 0xAA)).await.expect("write A");
                file.write_at(4 * SECTOR, &sectors(4, 0xBB))
                    .await
                    .expect("write B");
            })
            .await;

            sim.simulate_crash_for_process(test_ip(), true);

            let image = run_on(&mut sim, |provider| async move {
                read_image(&provider, "journal", 8 * SECTOR_SIZE).await
            })
            .await;
            if image.len() < 8 * SECTOR_SIZE {
                return false;
            }

            let b_intact = image[4 * SECTOR_SIZE..].iter().all(|byte| *byte == 0xBB);
            let a_sectors: Vec<bool> = (0..4)
                .map(|s| {
                    image[s * SECTOR_SIZE..(s + 1) * SECTOR_SIZE]
                        .iter()
                        .all(|byte| *byte == 0xAA)
                })
                .collect();
            let a_torn = a_sectors.iter().any(|kept| *kept) && a_sectors.iter().any(|kept| !kept);
            b_intact && a_torn
        });

        if torn {
            found = true;
            break;
        }
    }

    assert!(
        found,
        "no seed produced a later write surviving while an earlier one tore"
    );
}

/// A lost sector reads the file's fill pattern, and that pattern is zeros on
/// some files and garbage on others — content-sniffing recovery code must not
/// be able to hide behind either.
#[test]
fn lost_sectors_read_zeros_on_some_files_and_garbage_on_others() {
    let mut saw_zeros = false;
    let mut saw_garbage = false;

    for seed in 0..300_u64 {
        let fill = local_runtime().block_on(async {
            let config = StorageConfiguration {
                clean_crash_probability: 0.0,
                crash_lost_probability: 1.0,
                garbage_fill_probability: 0.5,
                ..StorageConfiguration::fast_local()
            };
            let mut sim = sim_with(seed, config);

            run_on(&mut sim, |provider| async move {
                let file = provider
                    .open("data", OpenOptions::create_write().read(true))
                    .await
                    .expect("open failed");
                file.write_at(0, &sectors(1, 0x5A)).await.expect("write");
            })
            .await;

            sim.simulate_crash_for_process(test_ip(), true);

            let image = run_on(&mut sim, |provider| async move {
                read_image(&provider, "data", SECTOR_SIZE).await
            })
            .await;
            if image.len() < SECTOR_SIZE || image.iter().all(|byte| *byte == 0x5A) {
                return None;
            }
            Some(image.iter().all(|byte| *byte == 0))
        });

        match fill {
            Some(true) => saw_zeros = true,
            Some(false) => saw_garbage = true,
            None => {}
        }
        if saw_zeros && saw_garbage {
            break;
        }
    }

    assert!(saw_zeros, "a lost sector must sometimes read as zeros");
    assert!(saw_garbage, "a lost sector must sometimes read as garbage");
}

/// A crash can roll back a contiguous run of sectors together, the way an
/// erase block fails as a unit.
#[test]
fn a_crash_can_roll_back_a_contiguous_run() {
    let mut found = false;

    for seed in 0..300_u64 {
        let run_length = local_runtime().block_on(async {
            let config = StorageConfiguration {
                clean_crash_probability: 0.0,
                correlated_rollback_probability: 1.0,
                ..StorageConfiguration::fast_local()
            };
            let mut sim = sim_with(seed, config);

            run_on(&mut sim, |provider| async move {
                let file = provider
                    .open("run", OpenOptions::create_write().read(true))
                    .await
                    .expect("open failed");
                file.set_len(8 * SECTOR).await.expect("set_len");
                file.sync_all().await.expect("sync");
                file.write_at(0, &sectors(8, 0x7E)).await.expect("write");
            })
            .await;

            sim.simulate_crash_for_process(test_ip(), true);

            let reports = sim.take_storage_crash_reports();
            reports
                .iter()
                .flat_map(|report| report.resolutions.iter())
                .filter(|resolution| resolution.correlated)
                .count()
        });

        if run_length >= 2 {
            found = true;
            break;
        }
    }

    assert!(found, "no seed rolled back a contiguous run of sectors");
}

/// Damage is deterministic: a corrupted sector reads back the *same* wrong
/// bytes every time. A retry never heals it.
#[test]
fn corruption_is_persistent_across_rereads() {
    local_runtime().block_on(async {
        let mut sim = sim_with(7, StorageConfiguration::fast_local());

        run_on(&mut sim, |provider| async move {
            let file = provider
                .open("page", OpenOptions::create_write().read(true))
                .await
                .expect("open failed");
            file.write_at(0, &sectors(1, 0x33)).await.expect("write");
            file.sync_all().await.expect("sync");
        })
        .await;

        sim.corrupt_file("page", 0..1).expect("corrupt failed");

        let (first, second) = run_on(&mut sim, |provider| async move {
            let file = provider
                .open("page", OpenOptions::read_only())
                .await
                .expect("open failed");
            let mut first = vec![0u8; SECTOR_SIZE];
            file.read_at(0, &mut first).await.expect("read failed");
            let mut second = vec![0u8; SECTOR_SIZE];
            file.read_at(0, &mut second).await.expect("read failed");
            (first, second)
        })
        .await;

        assert_ne!(
            first,
            sectors(1, 0x33),
            "a corrupted sector must not read back clean"
        );
        assert_eq!(first, second, "a retry must observe the identical damage");
    });
}

/// An I/O error and a successful read of damaged bytes are different
/// outcomes, and code has to tell them apart.
#[test]
fn eio_is_distinct_from_corrupt_but_readable_bytes() {
    local_runtime().block_on(async {
        let mut sim = sim_with(11, StorageConfiguration::fast_local());

        run_on(&mut sim, |provider| async move {
            let file = provider
                .open("pages", OpenOptions::create_write().read(true))
                .await
                .expect("open failed");
            file.write_at(0, &sectors(2, 0x44)).await.expect("write");
            file.sync_all().await.expect("sync");
        })
        .await;

        // Sector 0 errors; sector 1 succeeds and lies.
        sim.fail_file_with_eio("pages", 0..1, EioTarget::Read)
            .expect("eio failed");
        sim.corrupt_file("pages", 1..2).expect("corrupt failed");

        let (errored, corrupt) = run_on(&mut sim, |provider| async move {
            let file = provider
                .open("pages", OpenOptions::read_only())
                .await
                .expect("open failed");
            let mut buf = vec![0u8; SECTOR_SIZE];
            let errored = file.read_at(0, &mut buf).await.is_err();
            let read = file.read_at(SECTOR, &mut buf).await;
            (errored, read.map(|_| buf))
        })
        .await;

        assert!(errored, "a sector under EIO must fail the read");
        let corrupt = corrupt.expect("a corrupt sector still reads successfully");
        assert_ne!(corrupt, sectors(1, 0x44), "and it returns damaged bytes");

        let kinds: Vec<StorageFaultKind> = sim
            .take_storage_fault_records()
            .into_iter()
            .map(|record| record.kind)
            .collect();
        assert!(kinds.contains(&StorageFaultKind::EioRead));
    });
}

/// The oracle's own test: mutating a durable sector behind the model's back is
/// a simulator bug, and the next crash must say so rather than pass it off as
/// disk behaviour.
#[test]
#[should_panic(expected = "moonpool sim bug")]
fn the_oracle_catches_a_durable_sector_changing_underneath() {
    local_runtime().block_on(async {
        let mut sim = sim_with(3, StorageConfiguration::fast_local());

        run_on(&mut sim, |provider| async move {
            let file = provider
                .open("sb", OpenOptions::create_write())
                .await
                .expect("open failed");
            file.write_at(0, &sectors(1, 0x9C)).await.expect("write");
            file.sync_all().await.expect("sync");
        })
        .await;

        sim.corrupt_durable_out_of_band("sb", 0)
            .expect("out-of-band mutation failed");
        sim.simulate_crash_for_process(test_ip(), true);
    });
}

/// With the barrier-violation family armed, the same mismatch is expected
/// behaviour: a sync lied, the crash lost a write it had reported durable, and
/// the oracle reports it instead of failing the run.
#[test]
fn a_lying_sync_is_reported_rather_than_failing_the_run() {
    let mut found = false;

    for seed in 0..300_u64 {
        let lost = local_runtime().block_on(async {
            let config = StorageConfiguration {
                barrier_violation_probability: 1.0,
                clean_crash_probability: 0.0,
                ..StorageConfiguration::fast_local()
            };
            let mut sim = sim_with(seed, config);

            run_on(&mut sim, |provider| async move {
                let file = provider
                    .open("lying", OpenOptions::create_write().read(true))
                    .await
                    .expect("open failed");
                file.write_at(0, &sectors(1, 0x21)).await.expect("write");
                file.sync_all().await.expect("sync");
                // Overwrite and "sync" again: the second sync is the one that
                // has something to lie about.
                file.write_at(0, &sectors(1, 0x22)).await.expect("write");
                file.sync_all().await.expect("sync");
            })
            .await;

            sim.simulate_crash_for_process(test_ip(), true);

            sim.take_storage_fault_records()
                .iter()
                .any(|record| record.kind == StorageFaultKind::LostSyncedWrite)
        });

        if lost {
            found = true;
            break;
        }
    }

    assert!(
        found,
        "with barrier violation armed, a crash must sometimes report a lost synced write"
    );
}

/// The eligibility mask keeps damage away from sectors a replication-aware
/// harness has declared off limits, without moonpool knowing why.
#[test]
fn the_eligibility_mask_shields_the_sectors_it_names() {
    local_runtime().block_on(async {
        for seed in 0..40_u64 {
            let config = StorageConfiguration {
                clean_crash_probability: 0.0,
                crash_lost_probability: 1.0,
                crash_latent_fault_probability: 0.0,
                ..StorageConfiguration::fast_local()
            };
            let mut sim = sim_with(seed, config);
            // Only the second sector may ever be damaged.
            sim.set_storage_eligibility_mask(Arc::new(|_path: &str, sector: u64| sector != 0));

            run_on(&mut sim, |provider| async move {
                let file = provider
                    .open("replica", OpenOptions::create_write().read(true))
                    .await
                    .expect("open failed");
                file.write_at(0, &sectors(2, 0x6D)).await.expect("write");
            })
            .await;

            sim.simulate_crash_for_process(test_ip(), true);

            let damaged: Vec<u64> = sim
                .take_storage_crash_reports()
                .into_iter()
                .flat_map(|report| report.resolutions)
                .filter(|resolution| resolution.outcome == CrashOutcome::Lost)
                .map(|resolution| resolution.sector)
                .collect();
            assert!(
                !damaged.contains(&0),
                "seed {seed}: a masked-out sector must never be damaged"
            );
        }
    });
}

/// One seed, one post-crash image: the whole model is a pure function of the
/// seed and the operation sequence.
#[test]
fn the_same_seed_produces_the_same_post_crash_image() {
    let run_once = || {
        local_runtime().block_on(async {
            let config = StorageConfiguration {
                clean_crash_probability: 0.0,
                crash_lost_probability: 0.2,
                crash_latent_fault_probability: 0.2,
                garbage_fill_probability: 0.5,
                ..StorageConfiguration::fast_local()
            };
            let mut sim = sim_with(20_260_909, config);

            run_on(&mut sim, |provider| async move {
                let file = provider
                    .open("replay", OpenOptions::create_write().read(true))
                    .await
                    .expect("open failed");
                file.write_at(0, &sectors(4, 0x01)).await.expect("write");
                file.sync_all().await.expect("sync");
                file.write_at(0, &sectors(4, 0x02)).await.expect("write");
            })
            .await;

            sim.simulate_crash_for_process(test_ip(), true);

            run_on(&mut sim, |provider| async move {
                read_image(&provider, "replay", 4 * SECTOR_SIZE).await
            })
            .await
        })
    };

    assert_eq!(
        run_once(),
        run_once(),
        "the crash model must be a pure function of the seed"
    );
}
