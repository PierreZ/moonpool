use moonpool_sim::{SimWorld, network::config::NetworkConfiguration};
use std::{net::IpAddr, time::Duration};

/// Test basic partition functionality by directly testing the SimWorld API
#[test]
fn test_partition_api() {
    let sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());

    let client_ip: IpAddr = "127.0.0.1".parse().unwrap();
    let server_ip: IpAddr = "192.168.1.1".parse().unwrap();

    // Test that initially no partition exists
    assert!(!sim.is_partitioned(client_ip, server_ip).unwrap());

    // Create a partition between IPs
    sim.partition_pair(client_ip, server_ip, Duration::from_secs(10))
        .unwrap();

    // Verify partition is active
    assert!(sim.is_partitioned(client_ip, server_ip).unwrap());

    // Verify it's directional (server -> client should not be partitioned)
    assert!(!sim.is_partitioned(server_ip, client_ip).unwrap());

    // Test manual restoration
    sim.restore_partition(client_ip, server_ip).unwrap();
    assert!(!sim.is_partitioned(client_ip, server_ip).unwrap());
}

/// Test send partition functionality
#[test]
fn test_send_partition_api() {
    let sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());

    let client_ip: IpAddr = "127.0.0.1".parse().unwrap();
    let server_ip: IpAddr = "192.168.1.1".parse().unwrap();

    // Block all sends from client
    sim.partition_send_from(client_ip, Duration::from_secs(5))
        .unwrap();

    // Client should not be able to send to any IP
    assert!(sim.is_partitioned(client_ip, server_ip).unwrap());
    assert!(
        sim.is_partitioned(client_ip, "10.0.0.1".parse().unwrap())
            .unwrap()
    );

    // But server should still be able to send to client
    assert!(!sim.is_partitioned(server_ip, client_ip).unwrap());
}

/// Test receive partition functionality
#[test]
fn test_receive_partition_api() {
    let sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());

    let client_ip: IpAddr = "127.0.0.1".parse().unwrap();
    let server_ip: IpAddr = "192.168.1.1".parse().unwrap();

    // Block all receives to server
    sim.partition_recv_to(server_ip, Duration::from_secs(5))
        .unwrap();

    // Any IP should not be able to send to server
    assert!(sim.is_partitioned(client_ip, server_ip).unwrap());
    assert!(
        sim.is_partitioned("10.0.0.1".parse().unwrap(), server_ip)
            .unwrap()
    );

    // But server should still be able to send to others
    assert!(!sim.is_partitioned(server_ip, client_ip).unwrap());
}

/// Test automatic partition restoration through events
#[test]
fn test_automatic_partition_restoration() {
    let mut sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());

    let client_ip: IpAddr = "127.0.0.1".parse().unwrap();
    let server_ip: IpAddr = "192.168.1.1".parse().unwrap();

    // Very short partition for automatic restoration test
    sim.partition_pair(client_ip, server_ip, Duration::from_millis(50))
        .unwrap();

    // Verify partition is active
    assert!(sim.is_partitioned(client_ip, server_ip).unwrap());

    // Run simulation to process events
    sim.run_until_empty();

    // Partition should be automatically restored
    assert!(!sim.is_partitioned(client_ip, server_ip).unwrap());
}

/// Test multiple partition types simultaneously  
#[test]
fn test_multiple_partition_types() {
    let sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());

    let client_ip: IpAddr = "127.0.0.1".parse().unwrap();
    let server_ip: IpAddr = "192.168.1.1".parse().unwrap();

    // Apply both send and receive partitions
    sim.partition_send_from(client_ip, Duration::from_secs(10))
        .unwrap();
    sim.partition_recv_to(server_ip, Duration::from_secs(10))
        .unwrap();

    // Both should be in effect
    assert!(sim.is_partitioned(client_ip, server_ip).unwrap());

    // Even if we remove the send partition, receive partition should still block
    // (This tests that multiple partition types are checked)
    sim.restore_partition(client_ip, server_ip).unwrap(); // This won't affect send/recv partitions
    assert!(sim.is_partitioned(client_ip, server_ip).unwrap()); // Still blocked by recv partition
}

/// Test partition behavior - sends should fail during partitions
#[test]
fn test_partition_behavior() {
    let sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());

    let client_ip: IpAddr = "127.0.0.1".parse().unwrap();
    let server_ip: IpAddr = "192.168.1.1".parse().unwrap();

    // Create partition - sends should fail during partition
    sim.partition_pair(client_ip, server_ip, Duration::from_secs(10))
        .unwrap();
    assert!(sim.is_partitioned(client_ip, server_ip).unwrap());

    // Restore partition
    sim.restore_partition(client_ip, server_ip).unwrap();
    assert!(!sim.is_partitioned(client_ip, server_ip).unwrap());
}
