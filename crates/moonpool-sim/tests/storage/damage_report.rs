//! The damage report: each process sees what random faults and crashes
//! left damaged on its own disk, and nothing of another's (moonpool#295).

use std::net::IpAddr;

use crate::{local_runtime, run_as};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{DamageKind, DamagedRange, SECTOR_SIZE, SimWorld, StorageConfiguration};

const SECTORS: u64 = 4;

fn node_a() -> IpAddr {
    "10.0.1.1".parse().expect("valid IP")
}

fn node_b() -> IpAddr {
    "10.0.1.2".parse().expect("valid IP")
}

/// Write `byte` over every sector of `ip`'s `data` file, synced or not.
fn write(sim: &mut SimWorld, ip: IpAddr, byte: u8, sync: bool) {
    local_runtime().block_on(async {
        run_as(sim, ip, move |provider| async move {
            let file = provider
                .open("data", OpenOptions::create_write().read(true))
                .await?;
            let len = usize::try_from(SECTORS).expect("small") * SECTOR_SIZE;
            file.write_at(0, &vec![byte; len]).await?;
            if sync {
                file.sync_all().await?;
            }
            Ok::<_, std::io::Error>(())
        })
        .await
        .expect("write");
    });
}

/// `ip`'s report, read through its own provider.
fn report(sim: &mut SimWorld, ip: IpAddr) -> Vec<DamagedRange> {
    local_runtime().block_on(async {
        run_as(sim, ip, |provider| async move { provider.damaged() })
            .await
            .expect("report")
    })
}

#[test]
fn each_process_reports_only_its_own_damage() {
    let mut sim = SimWorld::new_with_seed(7);
    sim.set_storage_config(StorageConfiguration::fast_local());
    sim.set_process_storage_config(
        node_a(),
        StorageConfiguration {
            clean_crash_probability: 0.0,
            correlated_rollback_probability: 0.0,
            crash_lost_probability: 1.0,
            crash_latent_fault_probability: 0.0,
            shorn_write_probability: 0.0,
            length_survives_crash_probability: 1.0,
            ..StorageConfiguration::fast_local()
        },
    );
    sim.set_process_storage_config(
        node_b(),
        StorageConfiguration {
            write_corruption_probability: 1.0,
            ..StorageConfiguration::fast_local()
        },
    );

    // Both hold `data`; A's write is lost to a crash, B's is corrupted.
    write(&mut sim, node_a(), 0xA1, false);
    sim.simulate_crash_for_process(node_a(), true);
    write(&mut sim, node_b(), 0xB2, true);

    let lost = vec![DamagedRange {
        path: "data".to_string(),
        sectors: 0..SECTORS,
        kind: DamageKind::Lost,
    }];
    assert_eq!(report(&mut sim, node_a()), lost, "A sees only its crash");
    assert_eq!(sim.damaged(node_a()), lost, "the world agrees");
    let corrupt = vec![DamagedRange {
        path: "data".to_string(),
        sectors: 0..SECTORS,
        kind: DamageKind::Corrupt,
    }];
    assert_eq!(
        report(&mut sim, node_b()),
        corrupt,
        "B sees only its corruption"
    );

    // A rewrite of A's first sector clears it from A's report alone.
    sim.set_process_storage_config(node_a(), StorageConfiguration::fast_local());
    local_runtime().block_on(async {
        run_as(&mut sim, node_a(), |provider| async move {
            let file = provider.open("data", OpenOptions::read_write()).await?;
            file.write_at(0, &[0x5A; SECTOR_SIZE]).await
        })
        .await
        .expect("rewrite");
    });
    assert_eq!(
        report(&mut sim, node_a()),
        vec![DamagedRange {
            path: "data".to_string(),
            sectors: 1..SECTORS,
            kind: DamageKind::Lost,
        }]
    );
    assert_eq!(report(&mut sim, node_b()), corrupt, "B is untouched");
}

#[test]
fn the_report_is_read_only_and_draws_nothing() {
    let run = |ask: bool| {
        let mut sim = SimWorld::new_with_seed(11);
        sim.set_storage_config(StorageConfiguration {
            write_corruption_probability: 0.5,
            ..StorageConfiguration::fast_local()
        });
        write(&mut sim, node_a(), 1, true);
        if ask {
            let _ = report(&mut sim, node_a());
            let _ = sim.damaged(node_a());
        }
        write(&mut sim, node_a(), 2, true);
        report(&mut sim, node_a())
    };
    assert_eq!(run(false), run(true));
}
