//! Fault focus: a harness that knows its on-disk layout weighs where the
//! configured random faults land, through its own process's provider.

use crate::{local_runtime, run_on, test_ip};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{
    CrashOutcome, FaultFocus, SECTOR_SIZE, SimWorld, StorageConfiguration, StorageFaultKind,
};

const SECTOR: u64 = SECTOR_SIZE as u64;

fn sim_with(seed: u64, config: StorageConfiguration) -> SimWorld {
    let mut sim = SimWorld::new_with_seed(seed);
    sim.set_storage_config(config);
    sim
}

fn assert_weight(got: f64, want: f64, why: &str) {
    assert!((got - want).abs() < 1e-9, "{why}: {got} != {want}");
}

/// A spot weighs every sector it touches; overlapping spots take the
/// heaviest; anything else takes the background; paths are compared after
/// lexical normalization.
#[test]
fn a_focus_weighs_each_sector_by_its_heaviest_spot() {
    let focus = FaultFocus::new()
        .background(0.25)
        .spot("wal/seg", 100..600, 3.0)
        .spot("./wal//seg", 1024..1025, 8.0)
        .spot("wal/seg", 1000..1100, -1.0);
    assert_weight(focus.weight("wal/seg", 0), 3.0, "byte 100 is in sector 0");
    assert_weight(focus.weight("wal/seg", 1), 3.0, "byte 599 is in sector 1");
    assert_weight(focus.weight("wal/seg", 2), 8.0, "the heaviest spot wins");
    assert_weight(focus.weight("wal/seg", 3), 0.25, "uncovered: background");
    assert_weight(
        focus.weight("wal/other", 0),
        0.25,
        "another file: background",
    );
    assert_weight(
        FaultFocus::new().weight("x", 9),
        1.0,
        "a new focus is neutral",
    );
}

/// The damaging crash bands scale with the weight: with the background at
/// zero and one sector weighted to saturate them, every crash loses exactly
/// that sector.
#[test]
fn a_focus_aims_crash_damage_at_its_spot() {
    local_runtime().block_on(async {
        for seed in 0..30_u64 {
            let config = StorageConfiguration {
                clean_crash_probability: 0.0,
                correlated_rollback_probability: 0.0,
                crash_lost_probability: 0.05,
                crash_latent_fault_probability: 0.0,
                shorn_write_probability: 0.0,
                length_survives_crash_probability: 1.0,
                ..StorageConfiguration::fast_local()
            };
            let mut sim = sim_with(seed, config);
            run_on(&mut sim, |provider| async move {
                provider
                    .focus_faults(FaultFocus::new().background(0.0).spot(
                        "log",
                        3 * SECTOR..4 * SECTOR,
                        20.0,
                    ))
                    .expect("focus");
                let file = provider
                    .open("log", OpenOptions::create_write().read(true))
                    .await
                    .expect("open");
                file.write_at(0, &vec![0x5A; 16 * SECTOR_SIZE])
                    .await
                    .expect("write");
            })
            .await;
            sim.simulate_crash_for_process(test_ip(), true);
            let lost: Vec<u64> = sim
                .take_storage_crash_reports()
                .into_iter()
                .flat_map(|report| report.resolutions)
                .filter(|resolution| resolution.outcome == CrashOutcome::Lost)
                .map(|resolution| resolution.sector)
                .collect();
            assert_eq!(lost, vec![3], "seed {seed}");
        }
    });
}

/// Write-time corruption lands only where the focus allows it, and far more
/// often there than its configured rate.
#[test]
fn a_focus_concentrates_write_corruption() {
    local_runtime().block_on(async {
        let config = StorageConfiguration {
            write_corruption_probability: 0.001,
            ..StorageConfiguration::fast_local()
        };
        let mut sim = sim_with(7, config);
        run_on(&mut sim, |provider| async move {
            provider
                .focus_faults(FaultFocus::new().background(0.0).spot(
                    "log",
                    10 * SECTOR..12 * SECTOR,
                    100.0,
                ))
                .expect("focus");
            let file = provider
                .open("log", OpenOptions::create_write().read(true))
                .await
                .expect("open");
            for _ in 0..40 {
                file.write_at(0, &vec![0x11; 64 * SECTOR_SIZE])
                    .await
                    .expect("write");
            }
        })
        .await;
        let hits: Vec<_> = sim
            .take_storage_fault_records()
            .into_iter()
            .filter(|record| record.kind == StorageFaultKind::WriteCorruption)
            .collect();
        assert!(!hits.is_empty(), "the focused sectors are hit");
        for record in &hits {
            let sectors = record.sectors.clone().expect("a sector range");
            assert!(
                sectors.start <= 11 && sectors.end >= 10,
                "every corrupting write touched the spot: {record:?}"
            );
        }
    });
}

/// A neutral focus changes nothing: the same seed and operations produce
/// the same faults with and without one, and a wipe clears a focus.
#[test]
fn a_neutral_focus_is_no_focus_and_a_wipe_clears_one() {
    let run = |focus: Option<FaultFocus>| {
        local_runtime().block_on(async move {
            let config = StorageConfiguration {
                write_corruption_probability: 0.01,
                write_eio_probability: 0.01,
                clean_crash_probability: 0.0,
                crash_lost_probability: 0.1,
                ..StorageConfiguration::fast_local()
            };
            let mut sim = sim_with(11, config);
            if let Some(focus) = focus {
                sim.set_fault_focus(test_ip(), focus);
            }
            run_on(&mut sim, |provider| async move {
                let file = provider
                    .open("log", OpenOptions::create_write().read(true))
                    .await
                    .expect("open");
                for round in 0..20_u8 {
                    let _ = file.write_at(0, &vec![round; 8 * SECTOR_SIZE]).await;
                }
            })
            .await;
            sim.simulate_crash_for_process(test_ip(), true);
            let faults = sim.take_storage_fault_records();
            let crash = sim.take_storage_crash_reports();
            (format!("{faults:?}"), format!("{crash:?}"))
        })
    };
    let neutral = FaultFocus::new().spot("log", 0..4096, 1.0);
    assert_eq!(run(None), run(Some(neutral)));

    let mut sim = sim_with(1, StorageConfiguration::fast_local());
    sim.set_fault_focus(test_ip(), FaultFocus::new().background(0.0));
    sim.wipe_storage_for_process(test_ip());
    // After the wipe the focus is gone: crash damage lands again.
    sim.set_storage_config(StorageConfiguration {
        clean_crash_probability: 0.0,
        correlated_rollback_probability: 0.0,
        crash_lost_probability: 1.0,
        length_survives_crash_probability: 1.0,
        ..StorageConfiguration::fast_local()
    });
    local_runtime().block_on(async {
        run_on(&mut sim, |provider| async move {
            let file = provider
                .open("log", OpenOptions::create_write())
                .await
                .expect("open");
            file.write_at(0, &vec![1; SECTOR_SIZE])
                .await
                .expect("write");
        })
        .await;
    });
    sim.simulate_crash_for_process(test_ip(), true);
    let lost = sim
        .take_storage_crash_reports()
        .into_iter()
        .flat_map(|report| report.resolutions)
        .filter(|resolution| resolution.outcome == CrashOutcome::Lost)
        .count();
    assert_eq!(lost, 1, "the wiped disk's focus no longer shields it");
}

/// Any format's layout, listed as `LayoutRegion`s, weighs its regions by
/// kind — the simulator never learns the format.
#[test]
fn a_layout_is_weighed_by_its_kinds() {
    let regions = [
        moonpool_sim::LayoutRegion {
            path: "db/pages".to_string(),
            bytes: 0..512,
            kind: "superblock",
        },
        moonpool_sim::LayoutRegion {
            path: "db/pages".to_string(),
            bytes: 4096..8192,
            kind: "leaf",
        },
    ];
    let focus = FaultFocus::new()
        .background(0.0)
        .layout(&regions, |region| match region.kind {
            "superblock" => 10.0,
            _ => 2.0,
        });
    assert_weight(focus.weight("db/pages", 0), 10.0, "the superblock");
    assert_weight(focus.weight("db/pages", 9), 2.0, "a leaf page");
    assert_weight(focus.weight("db/pages", 3), 0.0, "between them: background");
    assert!(regions[1].overlaps("db/pages", &(8000..8001)));
    assert!(!regions[1].overlaps("db/other", &(8000..8001)));
}
