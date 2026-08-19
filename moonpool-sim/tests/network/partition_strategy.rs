//! Automatic partition rotation: strategy selection, including the
//! locality-shaped and one-way arms.
//!
//! Each test forces the rotation on (`partition_probability = 1.0`) with a
//! degenerate partition duration, so `sample_duration` consumes no RNG draw and
//! the only randomness left is the strategy's own selection.

use moonpool_sim::{
    LocalityInfo, NetworkProvider, SimFaultEvent, SimWorld, TcpListenerTrait,
    network::{NetworkConfiguration, PartitionStrategy},
};
use std::{collections::BTreeMap, net::IpAddr, time::Duration};

/// Fixed seed for every rotation test in this file.
const SEED: u64 = 42;

/// How many `step()` calls to give the rotation before declaring it silent.
const MAX_STEPS: usize = 20;

fn ip(last_octet: u8) -> IpAddr {
    format!("10.0.1.{last_octet}")
        .parse()
        .expect("valid test IP")
}

/// Network config with the automatic partition rotation forced on.
fn rotation_config(strategy: PartitionStrategy) -> NetworkConfiguration {
    let mut config = NetworkConfiguration::fast_local();
    config.chaos.partition_probability = 1.0;
    // Degenerate range: `sample_duration` returns `start` without an RNG draw,
    // and 30s outlives the microsecond-scale `fast_local` timings.
    config.chaos.partition_duration = Duration::from_secs(30)..Duration::from_secs(30);
    config.chaos.partition_strategy = strategy;
    config
}

/// Locality map for four processes laid out as `(datacenter, zone)` pairs,
/// assigned to `10.0.1.1` ..= `10.0.1.4`.
fn localities(layout: [(&str, &str); 4]) -> BTreeMap<IpAddr, LocalityInfo> {
    layout
        .iter()
        .enumerate()
        .map(|(index, (datacenter, zone))| {
            let last_octet = u8::try_from(index + 1).expect("four processes");
            let machine = format!("{zone}-m{last_octet}");
            (
                ip(last_octet),
                LocalityInfo::new(*datacenter, *zone, machine),
            )
        })
        .collect()
}

/// Build a simulation with two established connections, so the engine sees the
/// four IPs `10.0.1.1` ..= `10.0.1.4`, then step until the rotation fires.
///
/// Returns the simulation (for partition-state assertions) and the fault events
/// recorded by the triggering step.
fn run_rotation(
    strategy: PartitionStrategy,
    locality_map: BTreeMap<IpAddr, LocalityInfo>,
) -> (SimWorld, Vec<SimFaultEvent>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("failed to build test runtime");

    let mut sim = SimWorld::new_with_network_config_and_seed(rotation_config(strategy), SEED);
    sim.set_localities(locality_map);

    let faults = runtime.block_on(async {
        let first = sim.network_provider(ip(1));
        let listener_two = first.bind("10.0.1.2:8080").await.expect("bind .2");
        let _client_one = first
            .connect("10.0.1.2:8080")
            .await
            .expect("connect .1->.2");
        let (_server_two, _) = listener_two.accept().await.expect("accept on .2");

        let third = sim.network_provider(ip(3));
        let listener_four = third.bind("10.0.1.4:8080").await.expect("bind .4");
        let _client_three = third
            .connect("10.0.1.4:8080")
            .await
            .expect("connect .3->.4");
        let (_server_four, _) = listener_four.accept().await.expect("accept on .4");

        // Connections must stay alive while the rotation runs, otherwise the
        // engine has no IPs left to choose from.
        step_until_fault(&mut sim)
    });

    (sim, faults)
}

/// Step until the rotation records fault events, returning that step's faults
/// (empty if the rotation stayed silent for `MAX_STEPS`).
fn step_until_fault(sim: &mut SimWorld) -> Vec<SimFaultEvent> {
    for _ in 0..MAX_STEPS {
        sim.step();
        let faults: Vec<SimFaultEvent> = sim
            .take_faults()
            .into_iter()
            .map(|record| record.event)
            .collect();
        if !faults.is_empty() {
            return faults;
        }
    }
    Vec::new()
}

/// Assert that `a` and `b` cannot reach each other in either direction.
fn assert_cut(sim: &SimWorld, a: IpAddr, b: IpAddr) {
    assert!(
        sim.is_partitioned(a, b).expect("live simulation"),
        "expected {a} -> {b} to be partitioned"
    );
    assert!(
        sim.is_partitioned(b, a).expect("live simulation"),
        "expected {b} -> {a} to be partitioned"
    );
}

/// Assert that `a` and `b` still reach each other in both directions.
fn assert_intact(sim: &SimWorld, a: IpAddr, b: IpAddr) {
    assert!(
        !sim.is_partitioned(a, b).expect("live simulation"),
        "expected {a} -> {b} to stay reachable"
    );
    assert!(
        !sim.is_partitioned(b, a).expect("live simulation"),
        "expected {b} -> {a} to stay reachable"
    );
}

/// `IsolateZone` cuts the zone boundary and nothing else: both zones live in the
/// same datacenter, so only a zone-aware strategy produces this cut.
#[test]
fn isolate_zone_cuts_exactly_the_zone_boundary() {
    let layout = [
        ("dc1", "dc1-z1"),
        ("dc1", "dc1-z1"),
        ("dc1", "dc1-z2"),
        ("dc1", "dc1-z2"),
    ];
    let (sim, faults) = run_rotation(PartitionStrategy::IsolateZone, localities(layout));

    assert!(!faults.is_empty(), "rotation should have cut the zone");
    assert!(
        faults
            .iter()
            .all(|event| matches!(event, SimFaultEvent::PartitionCreated { .. })),
        "zone isolation should only record bidirectional partitions, got {faults:?}"
    );

    // Every cross-zone pair is cut in both directions.
    for &near in &[ip(1), ip(2)] {
        for &far in &[ip(3), ip(4)] {
            assert_cut(&sim, near, far);
        }
    }
    // Collocated processes keep talking.
    assert_intact(&sim, ip(1), ip(2));
    assert_intact(&sim, ip(3), ip(4));
}

/// `IsolateDatacenter` cuts one datacenter off while leaving its internal zone
/// boundaries intact, the difference from `IsolateZone` on the same topology.
#[test]
fn isolate_datacenter_keeps_sibling_zones_connected() {
    let layout = [
        ("dc1", "dc1-z1"),
        ("dc1", "dc1-z2"),
        ("dc2", "dc2-z1"),
        ("dc2", "dc2-z2"),
    ];
    let (sim, faults) = run_rotation(PartitionStrategy::IsolateDatacenter, localities(layout));

    assert!(
        !faults.is_empty(),
        "rotation should have cut the datacenter"
    );

    for &near in &[ip(1), ip(2)] {
        for &far in &[ip(3), ip(4)] {
            assert_cut(&sim, near, far);
        }
    }
    // Different zones, same datacenter: untouched by a datacenter-level cut.
    assert_intact(&sim, ip(1), ip(2));
    assert_intact(&sim, ip(3), ip(4));
}

/// `AsymmetricSend` silences one node's outgoing traffic while it keeps
/// receiving: the one-way failure that fools naive failure detectors.
#[test]
fn asymmetric_send_blocks_only_the_outgoing_direction() {
    let (sim, faults) = run_rotation(PartitionStrategy::AsymmetricSend, BTreeMap::new());

    let blocked = match faults.as_slice() {
        [SimFaultEvent::SendPartitionCreated { ip }] => {
            ip.parse::<IpAddr>().expect("fault carries a valid IP")
        }
        other => panic!("expected exactly one send partition fault, got {other:?}"),
    };

    for other in [ip(1), ip(2), ip(3), ip(4)] {
        if other == blocked {
            continue;
        }
        assert!(
            sim.is_partitioned(blocked, other).expect("live simulation"),
            "sends from {blocked} to {other} should be blocked"
        );
        assert!(
            !sim.is_partitioned(other, blocked).expect("live simulation"),
            "{other} should still reach {blocked}"
        );
    }
}

/// `AsymmetricRecv` is the mirror image: the node keeps sending, but hears
/// nothing back.
#[test]
fn asymmetric_recv_blocks_only_the_incoming_direction() {
    let (sim, faults) = run_rotation(PartitionStrategy::AsymmetricRecv, BTreeMap::new());

    let blocked = match faults.as_slice() {
        [SimFaultEvent::RecvPartitionCreated { ip }] => {
            ip.parse::<IpAddr>().expect("fault carries a valid IP")
        }
        other => panic!("expected exactly one recv partition fault, got {other:?}"),
    };

    for other in [ip(1), ip(2), ip(3), ip(4)] {
        if other == blocked {
            continue;
        }
        assert!(
            sim.is_partitioned(other, blocked).expect("live simulation"),
            "traffic from {other} to {blocked} should be blocked"
        );
        assert!(
            !sim.is_partitioned(blocked, other).expect("live simulation"),
            "{blocked} should still reach {other}"
        );
    }
}

/// Without a topology the locality-shaped arms must not panic: they degrade to
/// the flat `Random` selection, producing the exact same cut for the same seed.
#[test]
fn locality_strategies_degrade_to_random_without_topology() {
    let (_, random_faults) = run_rotation(PartitionStrategy::Random, BTreeMap::new());
    let (_, zone_faults) = run_rotation(PartitionStrategy::IsolateZone, BTreeMap::new());
    let (_, datacenter_faults) =
        run_rotation(PartitionStrategy::IsolateDatacenter, BTreeMap::new());

    assert!(
        !random_faults.is_empty(),
        "the reference `Random` rotation should have cut something"
    );
    let fingerprint = |faults: &[SimFaultEvent]| format!("{faults:?}");
    assert_eq!(
        fingerprint(&random_faults),
        fingerprint(&zone_faults),
        "IsolateZone without locality must behave like Random"
    );
    assert_eq!(
        fingerprint(&random_faults),
        fingerprint(&datacenter_faults),
        "IsolateDatacenter without locality must behave like Random"
    );
}
