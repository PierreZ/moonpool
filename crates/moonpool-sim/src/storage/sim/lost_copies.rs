//! The wipe ledger read by the storage engine: a wiped, unhealed process
//! counts as a lost copy in its replicated group.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::time::Duration;

use moonpool_core::{LayoutRegion, OpenOptions, StorageFile, StorageProvider};

use crate::chaos::state_handle::StateHandle;
use crate::runner::wipe::{WipeHeal, WipeLedger};
use crate::{
    CrashOutcome, DomainLevel, LocalityInfo, ReplicatedFaults, SECTOR_SIZE, SimWorld,
    StorageConfiguration,
};

const SECTOR: u64 = SECTOR_SIZE as u64;
const STRIPES: u64 = 12;

/// Three replicas, one per zone.
fn topology() -> BTreeMap<IpAddr, LocalityInfo> {
    ["a", "b", "c"]
        .into_iter()
        .enumerate()
        .map(|(at, zone)| (ip(at + 1), LocalityInfo::new("dc", zone, format!("m{at}"))))
        .collect()
}

fn ip(last: usize) -> IpAddr {
    format!("10.0.1.{last}").parse().expect("test IP")
}

/// One record per sector, the same stripe at the same offset everywhere.
fn layout() -> Vec<LayoutRegion> {
    (0..STRIPES)
        .map(|stripe| LayoutRegion {
            path: "replica".to_string(),
            bytes: stripe * SECTOR..(stripe + 1) * SECTOR,
            kind: "record",
            stripe: Some(stripe),
            shard: None,
        })
        .collect()
}

/// A world whose crashes lose every unsynced sector the pattern allows,
/// with the run's ledger installed as the runner installs it.
fn world(seed: u64) -> (SimWorld, StateHandle) {
    let mut sim = SimWorld::new_with_seed(seed);
    sim.set_storage_config(StorageConfiguration {
        clean_crash_probability: 0.0,
        correlated_rollback_probability: 0.0,
        crash_lost_probability: 1.0,
        crash_latent_fault_probability: 0.0,
        shorn_write_probability: 0.0,
        length_survives_crash_probability: 1.0,
        ..StorageConfiguration::fast_local()
    });
    let state = StateHandle::new();
    WipeLedger::install(&state);
    sim.set_wipe_ledger(WipeLedger::of(&state));
    let _pattern =
        sim.draw_replicated_faults(ReplicatedFaults::new(DomainLevel::Zone), &topology());
    (sim, state)
}

/// `owner` writes every record unsynced, then crashes: returns how many
/// sectors the crash lost.
fn lost_in_a_crash(sim: &mut SimWorld, owner: IpAddr) -> usize {
    sim.publish_layout(owner, &layout());
    let provider = sim.storage_provider(owner);
    crate::executor::Executor::new(0).block_on(sim.drive(async move {
        let file = provider
            .open("replica", OpenOptions::create_write())
            .await
            .expect("open");
        let len = usize::try_from(STRIPES).expect("small") * SECTOR_SIZE;
        file.write_at(0, &vec![0x5A; len])
            .await
            .expect("unsynced write");
    }));
    sim.simulate_crash_for_process(owner, true);
    sim.take_storage_crash_reports()
        .into_iter()
        .flat_map(|report| report.resolutions)
        .filter(|resolution| resolution.outcome == CrashOutcome::Lost)
        .count()
}

#[test]
fn a_wiped_unhealed_replica_keeps_damage_out_of_the_other_zones() {
    let mut damaged_elsewhere_before = false;
    for seed in 0..20 {
        let (mut sim, state) = world(seed);
        let ledger = WipeLedger::of(&state).expect("installed");
        // Zone b's replica was wiped and has not caught up.
        ledger.record_wipe(ip(2), WipeHeal::Signal);
        assert_eq!(
            sim.lost_in_replicated_groups(),
            BTreeSet::from([ip(2)]),
            "seed {seed}"
        );
        for other in [ip(1), ip(3)] {
            assert_eq!(
                lost_in_a_crash(&mut sim, other),
                0,
                "seed {seed}: {other} took damage while zone b holds no copy"
            );
        }
        // Healed, the pattern damages the other zones again.
        assert!(WipeLedger::heal(&state, "10.0.1.2"));
        assert!(sim.lost_in_replicated_groups().is_empty());
        damaged_elsewhere_before |= [ip(1), ip(3)]
            .into_iter()
            .any(|other| lost_in_a_crash(&mut sim, other) > 0);
    }
    assert!(
        damaged_elsewhere_before,
        "once healed, some seed damages zone a or c"
    );
}

#[test]
fn an_after_bound_heals_a_lost_copy() {
    let (sim, state) = world(1);
    let ledger = WipeLedger::of(&state).expect("installed");
    ledger.record_wipe(ip(2), WipeHeal::After(Duration::from_secs(5)));
    ledger.note_restart(ip(2), sim.current_time());
    assert_eq!(sim.lost_in_replicated_groups(), BTreeSet::from([ip(2)]));
    assert!(
        ledger
            .lost(sim.current_time() + Duration::from_secs(5))
            .is_empty(),
        "healed five seconds after the restart"
    );
}

#[test]
fn a_wipe_after_damage_in_another_zone_would_pass_the_tolerance() {
    let (mut sim, _state) = world(3);
    assert!(sim.wipe_keeps_tolerance(ip(2)), "no damage yet");
    let provider = sim.storage_provider(ip(1));
    sim.publish_layout(ip(1), &layout());
    crate::executor::Executor::new(0).block_on(sim.drive(async move {
        let file = provider
            .open("replica", OpenOptions::create_write())
            .await
            .expect("open");
        file.write_at(0, &[1; SECTOR_SIZE]).await.expect("write");
        file.sync_all().await.expect("sync");
        provider.sync_dir(".").await.expect("sync dir");
    }));
    sim.corrupt_process_file_bytes(ip(1), "replica", 0..SECTOR)
        .expect("corrupt");
    assert!(
        !sim.wipe_keeps_tolerance(ip(2)),
        "zone a holds damage: a wipe of zone b would leave stripe 0 no clean copy"
    );
    assert!(
        sim.wipe_keeps_tolerance(ip(1)),
        "wiping the damaged replica itself loses nothing more"
    );
}
