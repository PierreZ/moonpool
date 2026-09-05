//! Acceptance tests for the simulated block device (issue #184).
//!
//! Each test maps to one acceptance criterion of the barrier-bounded crash
//! model: reorder+tear, zero/garbage loss, correlated rollback, atomic
//! create, seed determinism, the lost-synced-write oracle, the fault
//! eligibility mask, and a Redwood-style two-persist commit.

use std::future::Future;
use std::sync::Arc;

use moonpool_core::block::SECTOR_SIZE;
use moonpool_core::{BlockDevice, BlockDeviceProvider, BlockError, RegionId, RegionSpec};
use moonpool_sim::{
    BlockCrashOutcome, BlockFaultConfig, BlockFaultKind, SimBlockDeviceProvider, SimBlockStore,
    executor::Executor,
};

const SECTOR: u64 = SECTOR_SIZE as u64;

fn run<F: Future>(seed: u64, future: F) -> F::Output {
    Executor::new(seed).block_on(future)
}

fn sectors(count: usize, byte: u8) -> Vec<u8> {
    vec![byte; count * SECTOR_SIZE]
}

/// Seed the simulation stream (the store's only randomness source) and
/// build a store over it.
fn store_with(seed: u64, config: BlockFaultConfig) -> (SimBlockStore, SimBlockDeviceProvider) {
    moonpool_sim::reset_sim_rng();
    moonpool_sim::set_sim_seed(seed);
    let store = SimBlockStore::new(config);
    let provider = SimBlockDeviceProvider::new(store.clone());
    (store, provider)
}

async fn read_sector(
    device: &impl BlockDevice,
    region: RegionId,
    sector: u64,
) -> Result<Vec<u8>, BlockError> {
    let mut buf = vec![0u8; SECTOR_SIZE];
    device.read(region, sector * SECTOR, &mut buf).await?;
    Ok(buf)
}

/// Criterion: `write(A); write(B); crash` yields, under some seed, B intact
/// while A is torn (per-sector mixed old/new) — the reorder+tear shape the
/// old stream-file model could not produce.
#[test]
fn crash_reorders_and_tears_unpersisted_writes() {
    let mut found = false;
    for seed in 0..2000 {
        let (store, provider) = store_with(seed, BlockFaultConfig::default());
        let hit = run(seed, async move {
            let device = provider
                .create(
                    "dev",
                    &[RegionSpec {
                        name: "data",
                        size: 64 * SECTOR,
                    }],
                )
                .await
                .expect("create");
            let region = RegionId(0);
            // Durable baseline so "old" is a recognizable pattern.
            device
                .write(region, 0, &sectors(4, 0xAA))
                .await
                .expect("baseline A");
            device
                .write(region, 10 * SECTOR, &sectors(2, 0xBB))
                .await
                .expect("baseline B");
            device.persist().await.expect("persist baseline");

            // A (earlier) and B (later), both unpersisted, then crash.
            device
                .write(region, 0, &sectors(4, 0x11))
                .await
                .expect("write A");
            device
                .write(region, 10 * SECTOR, &sectors(2, 0x22))
                .await
                .expect("write B");
            let _ = store.crash_device("dev");

            let mut a_old = 0;
            let mut a_new = 0;
            for sector in 0..4 {
                let buf = read_sector(&device, region, sector).await.expect("read A");
                if buf == sectors(1, 0xAA) {
                    a_old += 1;
                } else if buf == sectors(1, 0x11) {
                    a_new += 1;
                }
            }
            let mut b_new = 0;
            for sector in 10..12 {
                let buf = read_sector(&device, region, sector).await.expect("read B");
                if buf == sectors(1, 0x22) {
                    b_new += 1;
                }
            }
            // The later write fully landed while the earlier one is torn.
            b_new == 2 && a_old >= 1 && a_new >= 1
        });
        if hit {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "no seed produced B intact with A torn (reorder + per-sector tear)"
    );
}

/// Criterion: a lost sector resolves to zeros under one seed and to garbage
/// under another (fill mode is chosen per seed).
#[test]
fn lost_sectors_resolve_to_zeros_and_garbage_across_seeds() {
    let mut saw_zeros = false;
    let mut saw_garbage = false;
    for seed in 0..200 {
        let config = BlockFaultConfig {
            clean_crash_probability: 0.0,
            correlated_rollback_probability: 0.0,
            crash_lost_probability: 1.0,
            crash_latent_fault_probability: 0.0,
            ..BlockFaultConfig::default()
        };
        let (store, provider) = store_with(seed, config);
        let (lost, content) = run(seed, async move {
            let device = provider
                .create(
                    "dev",
                    &[RegionSpec {
                        name: "data",
                        size: 8 * SECTOR,
                    }],
                )
                .await
                .expect("create");
            let region = RegionId(0);
            device
                .write(region, 0, &sectors(1, 0xAA))
                .await
                .expect("baseline");
            device.persist().await.expect("persist");
            device
                .write(region, 0, &sectors(1, 0x11))
                .await
                .expect("overwrite");
            let report = store.crash_device("dev");
            let lost = report
                .resolutions
                .iter()
                .any(|r| r.sector == 0 && r.outcome == BlockCrashOutcome::Lost);
            let content = read_sector(&device, region, 0).await.expect("read");
            (lost, content)
        });
        assert!(lost, "crash_lost_probability=1.0 must lose the sector");
        assert_ne!(content, sectors(1, 0xAA), "lost sector must not read old");
        assert_ne!(content, sectors(1, 0x11), "lost sector must not read new");
        if content == vec![0u8; SECTOR_SIZE] {
            saw_zeros = true;
        } else {
            saw_garbage = true;
        }
        if saw_zeros && saw_garbage {
            return;
        }
    }
    panic!(
        "expected both zero-fill (saw: {saw_zeros}) and garbage-fill (saw: {saw_garbage}) seeds"
    );
}

/// Criterion: some seed exercises a correlated multi-sector rollback.
#[test]
fn correlated_rollback_rolls_back_a_contiguous_run() {
    let mut found = false;
    for seed in 0..100 {
        let config = BlockFaultConfig {
            clean_crash_probability: 0.0,
            correlated_rollback_probability: 1.0,
            ..BlockFaultConfig::default()
        };
        let (store, provider) = store_with(seed, config);
        let report = run(seed, async move {
            let device = provider
                .create(
                    "dev",
                    &[RegionSpec {
                        name: "data",
                        size: 16 * SECTOR,
                    }],
                )
                .await
                .expect("create");
            device.persist().await.expect("persist layout");
            device
                .write(RegionId(0), 0, &sectors(8, 0x11))
                .await
                .expect("write");
            store.crash_device("dev")
        });
        let correlated: Vec<u64> = report
            .resolutions
            .iter()
            .filter(|r| r.correlated)
            .map(|r| r.sector)
            .collect();
        let contiguous_pair = correlated.windows(2).any(|w| w[1] == w[0] + 1);
        if contiguous_pair
            && report
                .resolutions
                .iter()
                .filter(|r| r.correlated)
                .all(|r| r.outcome == BlockCrashOutcome::KeptOld)
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "no seed rolled back a contiguous run of sectors together"
    );
}

/// Criterion: `create()` + crash before first `persist()` leaves nothing
/// (`open()` fails `NotFound`); crash after the first `persist()` leaves the
/// full formatted layout.
#[test]
fn create_is_atomic_across_crashes() {
    let specs = [
        RegionSpec {
            name: "wal",
            size: 32 * SECTOR,
        },
        RegionSpec {
            name: "superblock",
            size: SECTOR,
        },
    ];
    let (store, provider) = store_with(7, BlockFaultConfig::default());
    run(7, async move {
        // Crash before the first persist: the device vanishes.
        let device = provider.create("dev", &specs).await.expect("create");
        device
            .write(RegionId(0), 0, &sectors(1, 0x11))
            .await
            .expect("write");
        let report = store.crash_device("dev");
        assert!(report.unlinked, "unpersisted device must vanish on crash");
        let err = provider.open("dev").await.expect_err("open must fail");
        assert!(matches!(err, BlockError::NotFound { .. }), "got {err:?}");

        // Create again, persist, crash: the full layout survives.
        let device = provider.create("dev", &specs).await.expect("re-create");
        device.persist().await.expect("persist");
        let report = store.crash_device("dev");
        assert!(!report.unlinked);
        let reopened = provider.open("dev").await.expect("open after persist");
        assert_eq!(reopened.region_count(), 2);
        assert_eq!(reopened.region_size(RegionId(0)), 32 * SECTOR);
        assert_eq!(reopened.region_size(RegionId(1)), SECTOR);
    });
}

/// Criterion: the same seed twice produces bit-identical post-crash state.
#[test]
fn same_seed_produces_bit_identical_post_crash_state() {
    let scenario = |seed: u64| {
        let config = BlockFaultConfig {
            eio_read_probability: 0.05,
            eio_write_probability: 0.05,
            read_corruption_probability: 0.05,
            misdirected_write_probability: 0.05,
            phantom_write_probability: 0.05,
            ..BlockFaultConfig::default()
        };
        let (store, provider) = store_with(seed, config);
        run(seed, async move {
            let device = provider
                .create(
                    "dev",
                    &[
                        RegionSpec {
                            name: "a",
                            size: 16 * SECTOR,
                        },
                        RegionSpec {
                            name: "b",
                            size: 8 * SECTOR,
                        },
                    ],
                )
                .await
                .expect("create");
            let _ = device.persist().await;
            for round in 0u8..4 {
                let _ = device
                    .write(
                        RegionId(0),
                        u64::from(round) * SECTOR,
                        &sectors(2, round | 0x10),
                    )
                    .await;
                let _ = device
                    .write(RegionId(1), 0, &sectors(1, round | 0x40))
                    .await;
                if round == 2 {
                    let _ = device.grow(RegionId(1), 12 * SECTOR).await;
                    let _ = device.persist().await;
                }
            }
            let report = store.crash_device("dev");
            let mut image = Vec::new();
            for region in [RegionId(0), RegionId(1)] {
                for sector in 0..device.region_size(region) / SECTOR {
                    // Faulted sectors corrupt deterministically, so the read
                    // image is part of the determinism contract too. Random
                    // EIO can fail a read; retries draw from the same seeded
                    // stream, so the retry pattern is deterministic as well.
                    let bytes = loop {
                        match read_sector(&device, region, sector).await {
                            Ok(bytes) => break bytes,
                            Err(BlockError::Io { .. }) => {}
                            Err(other) => panic!("read: {other:?}"),
                        }
                    };
                    image.extend(bytes);
                }
            }
            let outcomes: Vec<(RegionId, u64, BlockCrashOutcome)> = report
                .resolutions
                .iter()
                .map(|r| (r.region, r.sector, r.outcome))
                .collect();
            (image, outcomes)
        })
    };
    for seed in [3, 17, 99] {
        assert_eq!(scenario(seed), scenario(seed), "seed {seed} diverged");
    }
}

/// Criterion (oracle, hard-failure mode): an out-of-band mutation of a
/// persisted sector — a deliberately injected sim bug — fails loudly at the
/// next crash.
#[test]
fn oracle_panics_on_out_of_band_mutation_of_persisted_sector() {
    let (store, provider) = store_with(11, BlockFaultConfig::default());
    let store_for_crash = store.clone();
    run(11, async move {
        let device = provider
            .create(
                "dev",
                &[RegionSpec {
                    name: "data",
                    size: 8 * SECTOR,
                }],
            )
            .await
            .expect("create");
        device
            .write(RegionId(0), 0, &sectors(1, 0x55))
            .await
            .expect("write");
        device.persist().await.expect("persist");
    });
    store
        .corrupt_committed_out_of_band("dev", RegionId(0), 0)
        .expect("inject sim bug");
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = store_for_crash.crash_device("dev");
    }))
    .is_err();
    assert!(panicked, "oracle must fail loudly on a lost synced write");
}

/// Criterion (oracle, must-detect mode): with the barrier-violation family
/// armed, the same loss is reported as an expected fault event instead.
#[test]
fn oracle_detects_lost_synced_writes_when_barrier_violation_armed() {
    let config = BlockFaultConfig {
        barrier_violation_probability: 1.0,
        clean_crash_probability: 0.0,
        ..BlockFaultConfig::default()
    };
    let (store, provider) = store_with(13, config);
    let crash_store = store.clone();
    let report = run(13, async move {
        let device = provider
            .create(
                "dev",
                &[RegionSpec {
                    name: "data",
                    size: 8 * SECTOR,
                }],
            )
            .await
            .expect("create");
        device
            .write(RegionId(0), 0, &sectors(1, 0x55))
            .await
            .expect("baseline");
        device.persist().await.expect("persist baseline");
        device
            .write(RegionId(0), 0, &sectors(1, 0x66))
            .await
            .expect("overwrite");
        // persist() lies about every dirty sector (probability 1.0)...
        device.persist().await.expect("lying persist");
        // ...so the crash loses a write the caller was told is durable.
        crash_store.crash_device("dev")
    });
    assert_eq!(
        report.lost_synced,
        vec![(RegionId(0), 0)],
        "the oracle must detect exactly the lied-about sector"
    );
    let records = store.take_fault_records();
    assert!(
        records
            .iter()
            .any(|r| r.kind == BlockFaultKind::LostSyncedWrite),
        "the loss must be reported as an expected fault event; got {records:?}"
    );
}

/// Criterion: a mask forbidding faults in one region keeps that region
/// entirely fault-free across a sweep while the other region saturates.
#[test]
fn eligibility_mask_shields_a_region_across_a_sweep() {
    let protected = RegionId(0);
    let mut unprotected_faults = 0usize;
    let mut unprotected_damage = 0usize;
    for seed in 0..100 {
        let config = BlockFaultConfig {
            eio_read_probability: 0.3,
            eio_write_probability: 0.3,
            read_corruption_probability: 0.3,
            misdirected_write_probability: 0.3,
            phantom_write_probability: 0.3,
            clean_crash_probability: 0.0,
            crash_lost_probability: 0.3,
            crash_latent_fault_probability: 0.3,
            ..BlockFaultConfig::default()
        };
        let (store, provider) = store_with(seed, config);
        store.set_eligibility_mask(Arc::new(move |_path, region, _sector| region != protected));
        let crash_store = store.clone();
        let report = run(seed, async move {
            let device = provider
                .create(
                    "dev",
                    &[
                        RegionSpec {
                            name: "protected",
                            size: 8 * SECTOR,
                        },
                        RegionSpec {
                            name: "faulty",
                            size: 8 * SECTOR,
                        },
                    ],
                )
                .await
                .expect("create");
            let _ = device.persist().await;
            for region in [RegionId(0), RegionId(1)] {
                for sector in 0..4 {
                    let _ = device
                        .write(region, sector * SECTOR, &sectors(1, 0x11))
                        .await;
                    let _ = read_sector(&device, region, sector).await;
                }
            }
            crash_store.crash_device("dev")
        });
        for record in store.take_fault_records() {
            assert_ne!(
                record.region,
                Some(protected),
                "seed {seed}: fault event fired in the masked region: {record:?}"
            );
            if record.region == Some(RegionId(1)) {
                unprotected_faults += 1;
            }
        }
        for resolution in &report.resolutions {
            let damaging = matches!(
                resolution.outcome,
                BlockCrashOutcome::Lost | BlockCrashOutcome::LatentFault | BlockCrashOutcome::Shorn
            );
            if resolution.region == protected {
                assert!(
                    !damaging,
                    "seed {seed}: damaging crash outcome in the masked region: {resolution:?}"
                );
            } else if damaging {
                unprotected_damage += 1;
            }
        }
    }
    assert!(
        unprotected_faults > 0 && unprotected_damage > 0,
        "the unmasked region must keep saturating: {unprotected_faults} fault events, \
         {unprotected_damage} damaging outcomes"
    );
}

/// Criterion: a Redwood-style two-persist commit (write pages, persist, write
/// single-sector header, persist) recovers to either the old or the new
/// header under crash sweeps — never a torn committed state.
#[test]
fn two_persist_commit_never_leaves_a_torn_header() {
    for seed in 0..300 {
        // An AWUPF-compliant atomic-sector profile: buffered sectors resolve
        // strictly to old or new, which is the hardware assumption the
        // single-header protocol relies on.
        let config = BlockFaultConfig::default().atomic_sectors();
        let (store, provider) = store_with(seed, config);
        run(seed, async move {
            let pages = RegionId(0);
            let header = RegionId(1);
            let device = provider
                .create(
                    "dev",
                    &[
                        RegionSpec {
                            name: "pages",
                            size: 16 * SECTOR,
                        },
                        RegionSpec {
                            name: "header",
                            size: SECTOR,
                        },
                    ],
                )
                .await
                .expect("create");
            // Old committed state.
            device
                .write(pages, 0, &sectors(4, 0x01))
                .await
                .expect("old pages");
            device
                .write(header, 0, &sectors(1, 0xA1))
                .await
                .expect("old header");
            device.persist().await.expect("initial persist");

            // The two-persist commit protocol, crashed after `cut` steps.
            let cut = seed % 5;
            'protocol: {
                if cut == 0 {
                    break 'protocol;
                }
                device
                    .write(pages, 0, &sectors(4, 0x02))
                    .await
                    .expect("new pages");
                if cut == 1 {
                    break 'protocol;
                }
                device.persist().await.expect("persist pages");
                if cut == 2 {
                    break 'protocol;
                }
                device
                    .write(header, 0, &sectors(1, 0xB2))
                    .await
                    .expect("new header");
                if cut == 3 {
                    break 'protocol;
                }
                device.persist().await.expect("persist header");
            }
            let _ = store.crash_device("dev");

            // Recovery: the header must be exactly old or exactly new.
            let header_bytes = read_sector(&device, header, 0).await.expect("read header");
            let is_old = header_bytes == sectors(1, 0xA1);
            let is_new = header_bytes == sectors(1, 0xB2);
            assert!(
                is_old || is_new,
                "seed {seed} cut {cut}: torn committed header state"
            );
            // A committed new header implies the pages it points at are durable.
            if is_new {
                for sector in 0..4 {
                    let page = read_sector(&device, pages, sector)
                        .await
                        .expect("read page");
                    assert_eq!(
                        page,
                        sectors(1, 0x02),
                        "seed {seed} cut {cut}: new header with non-new page {sector}"
                    );
                }
            }
        });
    }
}

/// The contract's alignment and bounds rules are enforced.
#[test]
fn misaligned_and_out_of_bounds_operations_are_rejected() {
    let (_store, provider) = store_with(1, BlockFaultConfig::default());
    run(1, async move {
        let device = provider
            .create(
                "dev",
                &[RegionSpec {
                    name: "data",
                    size: 4 * SECTOR,
                }],
            )
            .await
            .expect("create");
        let region = RegionId(0);
        let mut buf = vec![0u8; SECTOR_SIZE];
        let misaligned = device.read(region, 1, &mut buf).await;
        assert!(matches!(
            misaligned,
            Err(BlockError::InvalidArgument { .. })
        ));
        let short = device.write(region, 0, &[0u8; 100]).await;
        assert!(matches!(short, Err(BlockError::InvalidArgument { .. })));
        let oob = device.write(region, 4 * SECTOR, &sectors(1, 0)).await;
        assert!(matches!(oob, Err(BlockError::InvalidArgument { .. })));
        let shrink = device.grow(region, 2 * SECTOR).await;
        assert!(matches!(shrink, Err(BlockError::InvalidArgument { .. })));
        device.grow(region, 8 * SECTOR).await.expect("grow");
        assert_eq!(device.region_size(region), 8 * SECTOR);
    });
}

/// Targeted fault API: EIO episodes fire on exactly the chosen sectors and
/// corruption is deterministic across retries (retries must not heal).
#[test]
fn targeted_faults_are_deterministic_and_scoped() {
    let (store, provider) = store_with(5, BlockFaultConfig::default());
    run(5, async move {
        let device = provider
            .create(
                "dev",
                &[RegionSpec {
                    name: "data",
                    size: 8 * SECTOR,
                }],
            )
            .await
            .expect("create");
        let region = RegionId(0);
        device
            .write(region, 0, &sectors(8, 0x77))
            .await
            .expect("write");
        device.persist().await.expect("persist");

        store
            .fail_with_eio("dev", region, 2..3, moonpool_sim::EioTarget::Read)
            .expect("arm eio");
        let err = read_sector(&device, region, 2).await.expect_err("eio");
        assert!(matches!(err, BlockError::Io { .. }));
        read_sector(&device, region, 1)
            .await
            .expect("other sectors fine");
        store
            .clear_eio("dev", region, moonpool_sim::EioTarget::Read)
            .expect("clear eio");
        read_sector(&device, region, 2)
            .await
            .expect("healed after clear");

        store.corrupt("dev", region, 4..5).expect("corrupt");
        let first = read_sector(&device, region, 4).await.expect("read corrupt");
        let second = read_sector(&device, region, 4)
            .await
            .expect("re-read corrupt");
        assert_ne!(first, sectors(1, 0x77), "corruption must be observable");
        assert_eq!(first, second, "retries must not heal corruption");
    });
}
