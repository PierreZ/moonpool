//! Placement by `(shard, stripe)`: damage to a region inside a declared
//! shard lands only while the shard's `durable` need holds on the copies
//! still clean.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use crate::{local_runtime, run_as};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::shard::{Need, Replica, Replication};
use moonpool_sim::{
    CrashOutcome, LayoutRegion, SECTOR_SIZE, ShardFaults, SimWorld, StorageConfiguration,
};

const SECTOR: u64 = SECTOR_SIZE as u64;
/// Stripes per shard.
const STRIPES: u64 = 16;

fn ips(count: u8) -> Vec<IpAddr> {
    (1..=count)
        .map(|at| format!("10.0.1.{at}").parse().expect("ip"))
        .collect()
}

fn replicas(ips: &[IpAddr]) -> Vec<Replica> {
    ips.iter().map(|ip| Replica::new(ip.to_string())).collect()
}

/// Shard `shards[i]` holds sectors `i * STRIPES..(i + 1) * STRIPES`, one
/// stripe each, rotated by the replica's rank so no two replicas share an
/// offset for one stripe.
fn layout(rank: u64, shards: &[&str]) -> Vec<LayoutRegion> {
    let mut regions = Vec::new();
    for (at, shard) in (0u64..).zip(shards) {
        for stripe in 0..STRIPES {
            let sector = at * STRIPES + (stripe + rank * 3) % STRIPES;
            regions.push(LayoutRegion {
                path: "replica".to_string(),
                bytes: sector * SECTOR..(sector + 1) * SECTOR,
                kind: "record",
                stripe: Some(stripe),
                shard: Some((*shard).to_string()),
            });
        }
    }
    regions
}

/// `(shard, stripe)` at `sector` for a replica of `rank`.
fn key_at(rank: u64, shards: &[&str], sector: u64) -> Option<(String, u64)> {
    layout(rank, shards)
        .into_iter()
        .find(|region| region.bytes.start == sector * SECTOR)
        .and_then(|region| Some((region.shard?, region.stripe?)))
}

/// The replicas that lost a copy, per `(shard, stripe)`.
type Lost = BTreeMap<(String, u64), BTreeSet<IpAddr>>;
/// The clean copies the book reports, per `(shard, stripe)`.
type Clean = BTreeMap<(String, u64), Vec<Replica>>;

/// One seed: declare `shards` with their replications, write every replica
/// without a sync and crash each in turn with lossy crashes. Returns the
/// replicas that lost a copy, per `(shard, stripe)`, and the clean copies
/// the book reports afterwards.
fn one_seed(seed: u64, replica_ips: &[IpAddr], shards: &[(&str, Replication)]) -> (Lost, Clean) {
    let mut sim = SimWorld::new_with_seed(seed);
    sim.set_storage_config(StorageConfiguration {
        clean_crash_probability: 0.0,
        correlated_rollback_probability: 0.0,
        crash_lost_probability: 0.6,
        crash_latent_fault_probability: 0.0,
        shorn_write_probability: 0.0,
        length_survives_crash_probability: 1.0,
        ..StorageConfiguration::fast_local()
    });
    let book = sim.install_shard_book(ShardFaults::default());
    for (name, replication) in shards {
        moonpool_sim::shard!(*name, current = replication.clone());
    }
    let names: Vec<&str> = shards.iter().map(|(name, _)| *name).collect();
    let mut lost = Lost::new();
    for (rank, ip) in (0u64..).zip(replica_ips) {
        let regions = layout(rank, &names);
        local_runtime().block_on(async {
            run_as(&mut sim, *ip, move |provider| async move {
                provider.publish_layout(&regions).expect("publish");
                let file = provider
                    .open("replica", OpenOptions::create_write().read(true))
                    .await
                    .expect("open");
                let len = usize::try_from(STRIPES).expect("small")
                    * SECTOR_SIZE
                    * regions
                        .len()
                        .div_ceil(usize::try_from(STRIPES).expect("small"));
                file.write_at(0, &vec![0x5A; len]).await.expect("write");
            })
            .await;
        });
    }
    // Crash every replica, in turn: each crash sees the damage the earlier
    // ones left.
    for (rank, ip) in (0u64..).zip(replica_ips) {
        sim.simulate_crash_for_process(*ip, true);
        for resolution in sim
            .take_storage_crash_reports()
            .into_iter()
            .flat_map(|report| report.resolutions)
            .filter(|resolution| resolution.outcome == CrashOutcome::Lost)
        {
            if let Some(key) = key_at(rank, &names, resolution.sector) {
                lost.entry(key).or_default().insert(*ip);
            }
        }
    }
    let mut clean = BTreeMap::new();
    for name in &names {
        for stripe in 0..STRIPES {
            clean.insert(
                ((*name).to_string(), stripe),
                book.clean_copies(name, stripe),
            );
        }
    }
    moonpool_sim::shard::clear_sink();
    (lost, clean)
}

/// A shard that tolerates one lost copy of three never loses two copies of
/// one stripe, and `clean_copies` reports the two that stay.
#[test]
fn a_shard_keeps_its_durable_copies_and_clean_copies_reports_them() {
    let nodes = ips(3);
    let shard = Replication::tolerates(replicas(&nodes), 1, 1);
    let mut damaged_somewhere = false;
    for seed in 0..24 {
        let (lost, clean) = one_seed(seed, &nodes, &[("s", shard.clone())]);
        for (key, replicas) in &lost {
            assert!(
                replicas.len() <= 1,
                "seed {seed}: {key:?} lost on {replicas:?}"
            );
        }
        for ((name, stripe), copies) in &clean {
            let lost_here = lost.get(&(name.clone(), *stripe)).map_or(0, BTreeSet::len);
            assert_eq!(
                copies.len(),
                3 - lost_here,
                "seed {seed}: clean copies of {name}/{stripe} match the crash reports"
            );
            assert!(copies.len() >= 2, "seed {seed}: {name}/{stripe}");
        }
        damaged_somewhere |= !lost.is_empty();
    }
    assert!(damaged_somewhere, "the crashes damaged some copy");
}

/// A grid-shaped need (each half of four replicas keeps a copy) never loses
/// a stripe, though a stripe may lose two copies.
#[test]
fn a_grid_shaped_durable_need_never_loses_a_stripe() {
    let nodes = ips(4);
    let durable = Need::All(vec![
        Need::AtLeast(1, replicas(&nodes[..2])),
        Need::AtLeast(1, replicas(&nodes[2..])),
    ]);
    let shard = Replication {
        replicas: replicas(&nodes),
        progress: Need::majority(replicas(&nodes)),
        durable: durable.clone(),
    };
    let mut two_lost = false;
    for seed in 0..24 {
        let (lost, clean) = one_seed(seed, &nodes, &[("grid", shard.clone())]);
        for (key, replicas) in &lost {
            let left = |half: &[IpAddr]| half.iter().any(|ip| !replicas.contains(ip));
            assert!(
                left(&nodes[..2]) && left(&nodes[2..]),
                "seed {seed}: {key:?} lost a whole half: {replicas:?}"
            );
            two_lost |= replicas.len() >= 2;
        }
        for (key, copies) in &clean {
            assert!(
                durable.holds(&|replica| copies.contains(replica)),
                "seed {seed}: {key:?} clean on {copies:?}"
            );
        }
    }
    assert!(two_lost, "a stripe lost one copy in each half on some seed");
}

/// Two shards on one machine take damage independently: the strict shard
/// never loses a copy, the loose one, with the same stripe numbers on the
/// same disks, does.
#[test]
fn two_shards_on_one_machine_take_independent_damage() {
    let nodes = ips(3);
    let strict = Replication::tolerates(replicas(&nodes), 1, 0);
    let loose = Replication::tolerates(replicas(&nodes), 1, 2);
    let mut loose_lost = false;
    for seed in 0..16 {
        let (lost, _) = one_seed(
            seed,
            &nodes,
            &[("strict", strict.clone()), ("loose", loose.clone())],
        );
        assert!(
            lost.keys().all(|(name, _)| name == "loose"),
            "seed {seed}: the strict shard lost a copy: {lost:?}"
        );
        loose_lost |= lost.values().any(|replicas| replicas.len() >= 2);
    }
    assert!(loose_lost, "the loose shard lost two copies of a stripe");
}
