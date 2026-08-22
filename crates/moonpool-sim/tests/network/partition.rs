use moonpool_sim::{SimFaultEvent, SimWorld, network::config::NetworkConfiguration};
use std::{net::IpAddr, time::Duration};

/// Test basic partition functionality by directly testing the `SimWorld` API
#[test]
fn test_partition_api() {
    let sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());

    let client_ip: IpAddr = "127.0.0.1".parse().unwrap();
    let server_ip: IpAddr = "192.168.1.1".parse().unwrap();

    // Test that initially no partition exists
    assert!(!sim.is_partitioned(client_ip, server_ip));

    // Create a partition between IPs
    sim.partition_pair(client_ip, server_ip, Duration::from_secs(10));

    // Verify partition is active
    assert!(sim.is_partitioned(client_ip, server_ip));

    // Verify it's directional (server -> client should not be partitioned)
    assert!(!sim.is_partitioned(server_ip, client_ip));

    // Test manual restoration
    sim.restore_partition(client_ip, server_ip);
    assert!(!sim.is_partitioned(client_ip, server_ip));
}

/// Test send partition functionality
#[test]
fn test_send_partition_api() {
    let sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());

    let client_ip: IpAddr = "127.0.0.1".parse().unwrap();
    let server_ip: IpAddr = "192.168.1.1".parse().unwrap();

    // Block all sends from client
    sim.partition_send_from(client_ip, Duration::from_secs(5));

    // Client should not be able to send to any IP
    assert!(sim.is_partitioned(client_ip, server_ip));
    assert!(sim.is_partitioned(client_ip, "10.0.0.1".parse().unwrap()));

    // But server should still be able to send to client
    assert!(!sim.is_partitioned(server_ip, client_ip));
}

/// Test receive partition functionality
#[test]
fn test_receive_partition_api() {
    let sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());

    let client_ip: IpAddr = "127.0.0.1".parse().unwrap();
    let server_ip: IpAddr = "192.168.1.1".parse().unwrap();

    // Block all receives to server
    sim.partition_recv_to(server_ip, Duration::from_secs(5));

    // Any IP should not be able to send to server
    assert!(sim.is_partitioned(client_ip, server_ip));
    assert!(sim.is_partitioned("10.0.0.1".parse().unwrap(), server_ip));

    // But server should still be able to send to others
    assert!(!sim.is_partitioned(server_ip, client_ip));
}

/// Test automatic partition restoration through events
#[test]
fn test_automatic_partition_restoration() {
    let mut sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());

    let client_ip: IpAddr = "127.0.0.1".parse().unwrap();
    let server_ip: IpAddr = "192.168.1.1".parse().unwrap();

    // Very short partition for automatic restoration test
    sim.partition_pair(client_ip, server_ip, Duration::from_millis(50));

    // Verify partition is active
    assert!(sim.is_partitioned(client_ip, server_ip));

    // Run simulation to process events
    sim.run_until_empty();

    // Partition should be automatically restored
    assert!(!sim.is_partitioned(client_ip, server_ip));
}

/// Test multiple partition types simultaneously  
#[test]
fn test_multiple_partition_types() {
    let sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());

    let client_ip: IpAddr = "127.0.0.1".parse().unwrap();
    let server_ip: IpAddr = "192.168.1.1".parse().unwrap();

    // Apply both send and receive partitions
    sim.partition_send_from(client_ip, Duration::from_secs(10));
    sim.partition_recv_to(server_ip, Duration::from_secs(10));

    // Both should be in effect
    assert!(sim.is_partitioned(client_ip, server_ip));

    // Even if we remove the send partition, receive partition should still block
    // (This tests that multiple partition types are checked)
    sim.restore_partition(client_ip, server_ip); // This won't affect send/recv partitions
    assert!(sim.is_partitioned(client_ip, server_ip)); // Still blocked by recv partition
}

/// Test partition behavior - sends should fail during partitions
#[test]
fn test_partition_behavior() {
    let sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());

    let client_ip: IpAddr = "127.0.0.1".parse().unwrap();
    let server_ip: IpAddr = "192.168.1.1".parse().unwrap();

    // Create partition - sends should fail during partition
    sim.partition_pair(client_ip, server_ip, Duration::from_secs(10));
    assert!(sim.is_partitioned(client_ip, server_ip));

    // Restore partition
    sim.restore_partition(client_ip, server_ip);
    assert!(!sim.is_partitioned(client_ip, server_ip));
}

#[test]
fn restoring_a_pair_heals_both_directions() {
    let sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());
    let first: IpAddr = "10.0.1.1".parse().unwrap();
    let second: IpAddr = "10.0.1.2".parse().unwrap();
    sim.partition_pair(first, second, Duration::from_secs(10));
    sim.partition_pair(second, first, Duration::from_secs(10));
    assert!(sim.is_partitioned(first, second));
    assert!(sim.is_partitioned(second, first));

    sim.restore_partition(first, second);

    assert!(!sim.is_partitioned(first, second));
    assert!(!sim.is_partitioned(second, first));
}

/// Test that fault events are recorded by the engine during partition
/// operations, in order. The engine records faults internally; the runner
/// (or a test, like here) drains them via `take_faults()`.
#[test]
fn test_partition_fault_timeline() {
    let sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());

    let a: IpAddr = "10.0.1.1".parse().unwrap();
    let b: IpAddr = "10.0.1.2".parse().unwrap();

    // Create and restore partition
    sim.partition_pair(a, b, Duration::from_secs(10));
    sim.restore_partition(a, b);

    // Also test directional partitions
    sim.partition_send_from(a, Duration::from_secs(5));
    sim.partition_recv_to(b, Duration::from_secs(5));

    // Drain the engine-recorded faults
    let faults = sim.take_faults();
    assert_eq!(faults.len(), 4, "should have 4 fault events");

    // Verify event types in order
    assert!(
        matches!(&faults[0].event, SimFaultEvent::PartitionCreated { from, to } if from == "10.0.1.1" && to == "10.0.1.2")
    );
    assert!(matches!(
        &faults[1].event,
        SimFaultEvent::PartitionHealed { .. }
    ));
    assert!(matches!(
        &faults[2].event,
        SimFaultEvent::SendPartitionCreated { ip } if ip == "10.0.1.1"
    ));
    assert!(matches!(
        &faults[3].event,
        SimFaultEvent::RecvPartitionCreated { ip } if ip == "10.0.1.2"
    ));

    // A second drain returns nothing
    assert!(sim.take_faults().is_empty());
}
