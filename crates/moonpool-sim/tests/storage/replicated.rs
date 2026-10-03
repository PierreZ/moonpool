//! Replicated fault patterns: storage damage spread over failure domains so
//! no replicated record is damaged in every domain.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use crate::{local_runtime, run_as};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{
    CrashOutcome, DomainLevel, FaultPattern, LayoutRegion, LocalityInfo, ReplicatedFaults,
    SECTOR_SIZE, SimWorld, StorageConfiguration,
};

const SECTOR: u64 = SECTOR_SIZE as u64;
/// Sector 0 holds node-local bytes; sectors `1..=STRIPES` one record each.
const STRIPES: u64 = 24;

/// Four replicas over three zones: two share zone `a`.
fn topology() -> BTreeMap<IpAddr, LocalityInfo> {
    [
        ("10.0.1.1", "a"),
        ("10.0.1.2", "a"),
        ("10.0.1.3", "b"),
        ("10.0.1.4", "c"),
    ]
    .into_iter()
    .enumerate()
    .map(|(at, (ip, zone))| {
        (
            ip.parse().expect("ip"),
            LocalityInfo::new("dc", zone, format!("m{at}")),
        )
    })
    .collect()
}

/// Each replica lays the records out at its own offsets (rotated by its
/// rank), as replicated logs do: a fault by offset could not keep a record
/// clean anywhere, a fault by stripe can.
fn layout(rank: u64) -> Vec<LayoutRegion> {
    let mut regions = vec![LayoutRegion {
        path: "replica".to_string(),
        bytes: 0..SECTOR,
        kind: "header",
        stripe: None,
    }];
    for stripe in 0..STRIPES {
        let sector = 1 + (stripe + rank * 5) % STRIPES;
        regions.push(LayoutRegion {
            path: "replica".to_string(),
            bytes: sector * SECTOR..(sector + 1) * SECTOR,
            kind: "record",
            stripe: Some(stripe),
        });
    }
    regions
}

/// The stripe a replica of `rank` keeps at `sector` (`None`: node-local).
fn stripe_at(rank: u64, sector: u64) -> Option<u64> {
    layout(rank)
        .into_iter()
        .find(|region| region.bytes.start == sector * SECTOR)
        .and_then(|region| region.stripe)
}

/// One seed: every replica writes and publishes its layout, then crashes;
/// returns the pattern and the zones each stripe (or node-local data) lost
/// a copy in.
fn one_seed(seed: u64) -> (FaultPattern, BTreeMap<Option<u64>, BTreeSet<String>>) {
    let mut sim = SimWorld::new_with_seed(seed);
    sim.set_storage_config(StorageConfiguration {
        clean_crash_probability: 0.0,
        correlated_rollback_probability: 0.0,
        crash_lost_probability: 0.5,
        crash_latent_fault_probability: 0.0,
        shorn_write_probability: 0.0,
        length_survives_crash_probability: 1.0,
        ..StorageConfiguration::fast_local()
    });
    let localities = topology();
    let pattern = sim.draw_replicated_faults(ReplicatedFaults::new(DomainLevel::Zone), &localities);
    let mut damaged: BTreeMap<Option<u64>, BTreeSet<String>> = BTreeMap::new();
    for (rank, (ip, locality)) in (0u64..).zip(&localities) {
        local_runtime().block_on(async {
            run_as(&mut sim, *ip, move |provider| async move {
                provider.publish_layout(&layout(rank)).expect("publish");
                let file = provider
                    .open("replica", OpenOptions::create_write().read(true))
                    .await
                    .expect("open");
                let len = usize::try_from(STRIPES + 1).expect("small") * SECTOR_SIZE;
                file.write_at(0, &vec![0x3C; len]).await.expect("write");
            })
            .await;
        });
        sim.simulate_crash_for_process(*ip, true);
        for resolution in sim
            .take_storage_crash_reports()
            .into_iter()
            .flat_map(|report| report.resolutions)
            .filter(|resolution| resolution.outcome == CrashOutcome::Lost)
        {
            damaged
                .entry(stripe_at(rank, resolution.sector))
                .or_default()
                .insert(locality.zone().to_string());
        }
    }
    (pattern, damaged)
}

/// Under every pattern no stripe, and no node-local data, loses copies in
/// more than one zone: stripes reach every zone, a minority only its own,
/// and a rolling turn only the first zone it damages, since no replica
/// repairs anything here.
#[test]
fn no_record_is_damaged_in_two_zones() {
    let (mut striped, mut minority, mut rolling) = (0, 0, 0);
    let (mut striped_everywhere, mut rolled) = (false, false);
    for seed in 0..40_u64 {
        let (pattern, damaged) = one_seed(seed);
        for (stripe, zones) in &damaged {
            assert!(
                zones.len() <= 1,
                "seed {seed}: {stripe:?} damaged in {zones:?} under {pattern:?}"
            );
        }
        let hit: BTreeSet<&String> = damaged.values().flatten().collect();
        match &pattern {
            FaultPattern::Striped { .. } => {
                striped += 1;
                striped_everywhere |= hit.len() == 3;
            }
            FaultPattern::Rolling { .. } => {
                rolling += 1;
                rolled |= hit.len() == 1;
                assert!(hit.len() <= 1, "seed {seed}: one turn, hit {hit:?}");
            }
            FaultPattern::Minority { domains } => {
                minority += 1;
                assert!(
                    hit.iter().all(|zone| domains.contains(zone)),
                    "seed {seed}: a minority damages only {domains:?}, hit {hit:?}"
                );
            }
            FaultPattern::Spared => panic!("three zones are enough"),
        }
    }
    assert!(
        striped > 0 && minority > 0 && rolling > 0,
        "every pattern is drawn"
    );
    assert!(striped_everywhere, "some seed's stripes reach every zone");
    assert!(rolled, "some seed's turn damages a zone");
}

/// Write every sector of `ip`'s replica and crash it; return the sectors
/// lost.
fn write_and_crash(sim: &mut SimWorld, ip: IpAddr, publish: Option<Vec<LayoutRegion>>) -> Vec<u64> {
    local_runtime().block_on(async {
        run_as(sim, ip, move |provider| async move {
            if let Some(regions) = publish {
                provider.publish_layout(&regions).expect("publish");
            }
            let file = provider
                .open("replica", OpenOptions::create_write().read(true))
                .await
                .expect("open");
            let len = usize::try_from(STRIPES + 1).expect("small") * SECTOR_SIZE;
            file.write_at(0, &vec![0x3C; len]).await.expect("write");
        })
        .await;
    });
    sim.simulate_crash_for_process(ip, true);
    sim.take_storage_crash_reports()
        .into_iter()
        .flat_map(|report| report.resolutions)
        .filter(|resolution| resolution.outcome == CrashOutcome::Lost)
        .map(|resolution| resolution.sector)
        .collect()
}

/// Under a stripe rotation, a replica outside the node-local domains loses only the
/// stripes that rotate to it; once a wipe forgets its layout, every byte
/// counts as node-local again, and it loses nothing.
#[test]
fn a_wipe_forgets_the_published_layout() {
    let localities = topology();
    for seed in 0..40_u64 {
        let mut sim = SimWorld::new_with_seed(seed);
        sim.set_storage_config(StorageConfiguration {
            clean_crash_probability: 0.0,
            correlated_rollback_probability: 0.0,
            crash_lost_probability: 1.0,
            length_survives_crash_probability: 1.0,
            ..StorageConfiguration::fast_local()
        });
        let FaultPattern::Striped { local, .. } =
            sim.draw_replicated_faults(ReplicatedFaults::new(DomainLevel::Zone), &localities)
        else {
            continue;
        };
        let Some((rank, (ip, _))) = (0u64..)
            .zip(&localities)
            .find(|(_, (_, locality))| !local.iter().any(|zone| zone == locality.zone()))
        else {
            continue;
        };
        let lost = write_and_crash(&mut sim, *ip, Some(layout(rank)));
        assert!(!lost.is_empty(), "seed {seed}: its own stripes are damaged");
        assert!(
            !lost.contains(&0),
            "seed {seed}: its node-local sector is spared"
        );
        sim.wipe_storage_for_process(*ip);
        let lost = write_and_crash(&mut sim, *ip, None);
        assert!(
            lost.is_empty(),
            "seed {seed}: after the wipe nothing is published: {lost:?}"
        );
        return;
    }
    panic!("no seed drew stripes with a replica outside the node-local zones");
}

/// Crash damage on every sector, no clean crash, no reverted length.
fn crash_loses_everything() -> StorageConfiguration {
    StorageConfiguration {
        clean_crash_probability: 0.0,
        correlated_rollback_probability: 0.0,
        crash_lost_probability: 1.0,
        length_survives_crash_probability: 1.0,
        ..StorageConfiguration::fast_local()
    }
}

/// A fresh world under [`crash_loses_everything`] whose seed draws a
/// rolling pattern over each of `groups`.
fn rolling_world(groups: &[BTreeMap<IpAddr, LocalityInfo>]) -> SimWorld {
    for seed in 0..200_u64 {
        let mut sim = SimWorld::new_with_seed(seed);
        sim.set_storage_config(crash_loses_everything());
        let all_rolling = groups.iter().all(|members| {
            matches!(
                sim.draw_replicated_faults(ReplicatedFaults::new(DomainLevel::Zone), members),
                FaultPattern::Rolling { .. }
            )
        });
        if all_rolling {
            return sim;
        }
    }
    panic!("no seed drew a rolling pattern for every group");
}

/// Rewrite every sector of `ip`'s replica in place (no truncation),
/// leaving the bytes unsynced.
fn repair(sim: &mut SimWorld, ip: IpAddr) {
    local_runtime().block_on(async {
        run_as(sim, ip, move |provider| async move {
            let file = provider
                .open("replica", OpenOptions::read_write())
                .await
                .expect("open");
            let len = usize::try_from(STRIPES + 1).expect("small") * SECTOR_SIZE;
            file.write_at(0, &vec![0x5A; len]).await.expect("write");
        })
        .await;
    });
}

fn ip(last: u8) -> IpAddr {
    IpAddr::from([10, 0, 1, last])
}

/// The turn stays on the zone holding damage, and moves on once that
/// damage is rewritten: zone `b` is damaged, zone `c` is spared until `b`
/// repairs its replica, then `c` takes the turn and the damage.
#[test]
fn a_rolling_turn_moves_on_once_the_damage_is_repaired() {
    let mut sim = rolling_world(&[topology()]);
    assert!(sim.fault_turn(ip(3)).len() == 1, "one zone holds the turn");
    let lost_b = write_and_crash(&mut sim, ip(3), Some(layout(2)));
    assert!(!lost_b.is_empty(), "zone b takes the turn and the damage");
    assert_eq!(sim.fault_turn(ip(3)), vec!["b".to_string()]);
    let lost_c = write_and_crash(&mut sim, ip(4), Some(layout(3)));
    assert!(lost_c.is_empty(), "zone c waits for b: {lost_c:?}");
    repair(&mut sim, ip(3));
    let lost_c = write_and_crash(&mut sim, ip(4), Some(layout(3)));
    assert!(!lost_c.is_empty(), "b repaired, c takes the turn");
    assert_eq!(sim.fault_turn(ip(4)), vec!["c".to_string()]);
}

/// Truncating a damaged file away repairs it too.
#[test]
fn a_truncation_repairs_the_damage() {
    let mut sim = rolling_world(&[topology()]);
    assert!(!write_and_crash(&mut sim, ip(3), Some(layout(2))).is_empty());
    local_runtime().block_on(async {
        run_as(&mut sim, ip(3), |provider| async move {
            let file = provider
                .open("replica", OpenOptions::read_write())
                .await
                .expect("open");
            file.set_len(0).await.expect("truncate");
        })
        .await;
    });
    assert!(
        !write_and_crash(&mut sim, ip(4), Some(layout(3))).is_empty(),
        "b truncated its damage away, c takes the turn"
    );
}

/// Damage outside every region a replica publishes does not hold the
/// turn: republishing without the damaged sectors frees it.
#[test]
fn unpublished_damage_does_not_hold_the_turn() {
    let mut sim = rolling_world(&[topology()]);
    let lost_b = write_and_crash(&mut sim, ip(3), Some(layout(2)));
    assert!(!lost_b.is_empty(), "zone b takes the damage");
    local_runtime().block_on(async {
        run_as(&mut sim, ip(3), |provider| async move {
            provider.publish_layout(&[]).expect("publish");
        })
        .await;
    });
    let lost_c = write_and_crash(&mut sim, ip(4), Some(layout(3)));
    assert!(
        !lost_c.is_empty(),
        "nothing published holds damage, c takes the turn"
    );
}

/// Two groups over the same zones roll independent turns: each holds
/// damage in its own zone at the same time.
#[test]
fn each_group_rolls_its_own_turn() {
    let topology = topology();
    let group = |members: [u8; 2]| -> BTreeMap<IpAddr, LocalityInfo> {
        topology
            .iter()
            .filter(|(at, _)| members.iter().any(|last| **at == ip(*last)))
            .map(|(at, locality)| (*at, locality.clone()))
            .collect()
    };
    // Group one: zones a and b. Group two: zones a and c.
    let mut sim = rolling_world(&[group([1, 3]), group([2, 4])]);
    assert!(!write_and_crash(&mut sim, ip(3), Some(layout(2))).is_empty());
    assert!(!write_and_crash(&mut sim, ip(4), Some(layout(3))).is_empty());
    assert_eq!(sim.fault_turn(ip(3)), vec!["b".to_string()]);
    assert_eq!(sim.fault_turn(ip(4)), vec!["c".to_string()]);
    assert!(
        write_and_crash(&mut sim, ip(1), Some(layout(0))).is_empty(),
        "zone a waits in group one"
    );
}
