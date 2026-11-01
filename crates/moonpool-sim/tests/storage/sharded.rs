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

/// A harness that damages bytes or gives up a whole replica from inside a
/// process asks the same gate: the second copy of one stripe is refused,
/// and so is a replica loss that would leave a stripe one clean copy.
#[test]
fn in_process_damage_and_replica_loss_ask_the_gate() {
    let nodes = ips(3);
    let mut sim = SimWorld::new_with_seed(7);
    let book = sim.install_shard_book(ShardFaults::default());
    moonpool_sim::shard!(
        "s",
        current = Replication::tolerates(replicas(&nodes), 1, 1)
    );
    for (rank, ip) in (0u64..).zip(&nodes) {
        let regions = layout(rank, &["s"]);
        local_runtime().block_on(async {
            run_as(&mut sim, *ip, move |provider| async move {
                provider.publish_layout(&regions).expect("publish");
                let file = provider
                    .open("replica", OpenOptions::create_write().read(true))
                    .await
                    .expect("open");
                let len = usize::try_from(STRIPES).expect("small") * SECTOR_SIZE;
                file.write_at(0, &vec![0x5A; len]).await.expect("write");
                file.sync_all().await.expect("sync");
            })
            .await;
        });
    }
    // Stripe 0's sector on each replica.
    let at = |rank: u64| {
        let sector = (rank * 3) % STRIPES;
        sector * SECTOR..sector * SECTOR + 8
    };
    let first = sim.storage_provider(nodes[0]);
    let second = sim.storage_provider(nodes[1]);
    assert!(first.corrupt_bytes("replica", at(0)).expect("corrupt"));
    assert!(!second.corrupt_bytes("replica", at(1)).expect("corrupt"));
    assert!(
        !second.lose_replica().expect("lose"),
        "stripe 0 keeps two copies"
    );
    assert_eq!(book.clean_copies("s", 0).len(), 2);
    assert!(
        first.lose_replica().expect("lose"),
        "its own damage costs nothing more"
    );
    assert!(
        !second.lose_replica().expect("lose"),
        "one replica is gone already"
    );
    moonpool_sim::shard::clear_sink();
}

/// A replica the process found lost for good is lost to the gate too, even
/// when the gate let the fault land on clean bytes: the next loss in the
/// shard is refused.
#[test]
fn a_replica_lost_past_the_gate_counts_against_the_shard() {
    let nodes = ips(3);
    let mut sim = SimWorld::new_with_seed(11);
    let book = sim.install_shard_book(ShardFaults::default());
    moonpool_sim::shard!(
        "s",
        current = Replication::tolerates(replicas(&nodes), 1, 1)
    );
    for (rank, ip) in (0u64..).zip(&nodes) {
        let regions = layout(rank, &["s"]);
        local_runtime().block_on(async {
            run_as(&mut sim, *ip, move |provider| async move {
                provider.publish_layout(&regions).expect("publish");
                let file = provider
                    .open("replica", OpenOptions::create_write().read(true))
                    .await
                    .expect("open");
                let len = usize::try_from(STRIPES).expect("small") * SECTOR_SIZE;
                file.write_at(0, &vec![0x5A; len]).await.expect("write");
                file.sync_all().await.expect("sync");
            })
            .await;
        });
    }
    assert_eq!(book.clean_copies("s", 0).len(), 3);
    let first = sim.storage_provider(nodes[0]);
    let second = sim.storage_provider(nodes[1]);
    first.lose_replica_past_gate().expect("lose");
    assert_eq!(
        book.clean_copies("s", 0).len(),
        2,
        "the lost copy is not clean"
    );
    assert!(
        !second.lose_replica().expect("lose"),
        "one replica is gone already"
    );
    moonpool_sim::shard::clear_sink();
}

/// A copy lost in one shard counts against that shard alone: the replica's
/// copy of another shard stays clean, and the lost copy heals once it
/// catches up.
#[test]
fn a_shard_copy_lost_past_the_gate_counts_against_that_shard_alone() {
    let nodes = ips(3);
    let mut sim = SimWorld::new_with_seed(12);
    let book = sim.install_shard_book(ShardFaults::default());
    moonpool_sim::shard!(
        "s",
        current = Replication::tolerates(replicas(&nodes), 1, 1)
    );
    moonpool_sim::shard!(
        "t",
        current = Replication::tolerates(replicas(&nodes), 1, 1)
    );
    for (rank, ip) in (0u64..).zip(&nodes) {
        let regions = layout(rank, &["s", "t"]);
        local_runtime().block_on(async {
            run_as(&mut sim, *ip, move |provider| async move {
                provider.publish_layout(&regions).expect("publish");
                let file = provider
                    .open("replica", OpenOptions::create_write().read(true))
                    .await
                    .expect("open");
                let len = 2 * usize::try_from(STRIPES).expect("small") * SECTOR_SIZE;
                file.write_at(0, &vec![0x5A; len]).await.expect("write");
                file.sync_all().await.expect("sync");
            })
            .await;
        });
    }
    let first = sim.storage_provider(nodes[0]);
    let second = sim.storage_provider(nodes[1]);
    first.lose_shard_past_gate("s").expect("lose");
    assert_eq!(
        book.clean_copies("s", 0).len(),
        2,
        "the lost copy is not clean"
    );
    assert_eq!(
        book.clean_copies("t", 0).len(),
        3,
        "the other shard keeps its copy"
    );
    assert!(
        !second.lose_replica().expect("lose"),
        "one copy of s is gone already"
    );
    assert!(book.caught_up("s", &Replica::new(nodes[0].to_string())));
    assert_eq!(book.clean_copies("s", 0).len(), 3, "the copy healed");
    moonpool_sim::shard::clear_sink();
}

/// One replica's whole copy of `shard`: a region with no stripe, so the
/// copy stands or falls whole.
fn whole_layout(shard: &str) -> Vec<LayoutRegion> {
    vec![LayoutRegion {
        path: "registry".to_string(),
        bytes: 0..4 * SECTOR,
        kind: "registry",
        stripe: None,
        shard: Some(shard.to_string()),
    }]
}

/// Write `nodes`' whole copies of `s` without a sync, then crash the first
/// under `config`. Returns the world, its shards and the replicas.
fn crash_a_whole_copy(
    config: StorageConfiguration,
) -> (SimWorld, moonpool_sim::Shards, Vec<IpAddr>) {
    let nodes = ips(3);
    let mut sim = SimWorld::new_with_seed(7);
    sim.set_storage_config(config);
    let book = sim.install_shard_book(ShardFaults::default());
    moonpool_sim::shard!(
        "s",
        current = Replication::tolerates(replicas(&nodes), 1, 1)
    );
    for ip in &nodes {
        let regions = whole_layout("s");
        local_runtime().block_on(async {
            run_as(&mut sim, *ip, move |provider| async move {
                provider.publish_layout(&regions).expect("publish");
                let file = provider
                    .open("registry", OpenOptions::create_write().read(true))
                    .await
                    .expect("open");
                file.write_at(0, &vec![0x5A; 4 * SECTOR_SIZE])
                    .await
                    .expect("write");
            })
            .await;
        });
    }
    sim.simulate_crash_for_process(nodes[0], true);
    (sim, book, nodes)
}

/// A crash that rolls back a dirty sector of a whole copy (a torn tail)
/// counts that copy lost until its store opens it intact; a crash that
/// keeps every write counts nothing.
#[test]
fn a_crash_that_tears_a_whole_copy_counts_it_lost_until_it_opens_intact() {
    let (_sim, book, nodes) = crash_a_whole_copy(StorageConfiguration {
        clean_crash_probability: 0.0,
        correlated_rollback_probability: 1.0,
        crash_lost_probability: 0.0,
        crash_latent_fault_probability: 0.0,
        shorn_write_probability: 0.0,
        length_survives_crash_probability: 1.0,
        ..StorageConfiguration::fast_local()
    });
    assert_eq!(
        book.clean_copies("s", 0).len(),
        2,
        "the torn copy is not clean"
    );
    // Its store opens the copy, checks it and keeps it.
    assert!(book.intact(nodes[0], "s"));
    assert!(!book.intact(nodes[0], "s"), "nothing more to heal");
    assert_eq!(book.clean_copies("s", 0).len(), 3, "the copy opened intact");
    moonpool_sim::shard::clear_sink();

    let (_sim, book, _) = crash_a_whole_copy(StorageConfiguration {
        clean_crash_probability: 1.0,
        ..StorageConfiguration::fast_local()
    });
    assert_eq!(
        book.clean_copies("s", 0).len(),
        3,
        "a clean crash tears nothing"
    );
    moonpool_sim::shard::clear_sink();
}
