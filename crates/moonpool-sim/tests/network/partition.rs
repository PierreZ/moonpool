use futures::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    task::noop_waker,
};
use moonpool_sim::{
    NetworkProvider, SimFaultEvent, SimWorld, TcpListenerTrait,
    network::config::NetworkConfiguration,
};
use std::{
    future::Future,
    io,
    net::IpAddr,
    pin::{Pin, pin},
    task::{Context, Poll},
    time::Duration,
};

/// How many events a driven future may consume before it is declared stuck.
const MAX_DRIVER_STEPS: usize = 10_000;

fn ip(last_octet: u8) -> IpAddr {
    format!("10.0.1.{last_octet}")
        .parse()
        .expect("valid test IP")
}

/// Poll a simulation-backed future, advancing virtual time whenever it parks.
fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);

    for _ in 0..MAX_DRIVER_STEPS {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        assert!(
            sim.has_pending_events(),
            "simulation-backed future stalled without a pending event"
        );
        sim.step();
    }

    panic!("simulation-backed future exceeded {MAX_DRIVER_STEPS} events")
}

fn poll_write_once(stream: &mut (impl AsyncWrite + Unpin), data: &[u8]) -> Poll<io::Result<usize>> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    Pin::new(stream).poll_write(&mut context, data)
}

fn poll_read_once(
    stream: &mut (impl AsyncRead + Unpin),
    data: &mut [u8],
) -> Poll<io::Result<usize>> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    Pin::new(stream).poll_read(&mut context, data)
}

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

/// A partition must never punch a hole in an established byte stream.
///
/// Regression for the interior-hole bug: the engine used to drop the queued
/// chunk of a partitioned connection while leaving later chunks flowing, so the
/// peer silently read the suffix in place of the missing range. A framed
/// protocol (h2 on top of this transport) then parsed later traffic as the
/// remainder of an earlier frame.
#[test]
fn a_partition_between_chunks_never_leaves_a_hole_in_the_stream() {
    const FIRST: &[u8] = b"first-chunk-of-the-frame";
    const SECOND: &[u8] = b"second-chunk-of-the-frame";
    const PARTITION: Duration = Duration::from_millis(200);

    let mut sim =
        SimWorld::new_with_network_config_and_seed(NetworkConfiguration::fast_local(), 20_240_183);
    let client_ip = ip(1);
    let server_ip = ip(2);

    let server_provider = sim.network_provider(server_ip);
    let listener = drive(&mut sim, server_provider.bind("10.0.1.2:8080")).expect("bind");
    let client_provider = sim.network_provider(client_ip);
    let mut client = drive(&mut sim, client_provider.connect("10.0.1.2:8080")).expect("connect");
    let (mut server, _) = drive(&mut sim, listener.accept()).expect("accept");
    sim.run_until_empty();
    assert_eq!(sim.pending_event_count(), 0, "the handshake has settled");

    let capacity = sim.available_send_bytes(client.connection_id());
    assert_eq!(
        poll_write_once(&mut client, FIRST).map(|result| result.expect("first chunk is accepted")),
        Poll::Ready(FIRST.len())
    );

    // The partition strikes after the chunk is queued but before the engine
    // drains it: exactly the window that used to swallow the bytes.
    sim.partition_pair(client_ip, server_ip, PARTITION);
    assert!(sim.step(), "the engine drains the queued send");

    assert_eq!(
        sim.available_send_bytes(client.connection_id()),
        capacity - FIRST.len(),
        "a partitioned send holds its bytes instead of dropping them"
    );
    let mut byte = [0_u8; 1];
    assert!(
        poll_read_once(&mut server, &mut byte).is_pending(),
        "no byte crosses an active partition"
    );

    // Healing releases the stalled chunk, and the next chunk follows it.
    sim.run_until_empty();
    assert!(
        !sim.is_partitioned(client_ip, server_ip),
        "partition healed"
    );
    assert_eq!(
        poll_write_once(&mut client, SECOND)
            .map(|result| result.expect("second chunk is accepted")),
        Poll::Ready(SECOND.len())
    );
    sim.run_until_empty();

    let mut received = vec![0_u8; FIRST.len() + SECOND.len()];
    drive(&mut sim, server.read_exact(&mut received)).expect("both chunks arrive");
    assert_eq!(
        received,
        [FIRST, SECOND].concat(),
        "the peer reads the original bytes in order, with no interior hole"
    );
}

/// Healing a partition early releases the bytes it stalled, right away.
///
/// The stalled stream is re-driven by the heal itself, not by the deadline it
/// stalled under: a `FaultContext::heal_partition` (which cuts for an hour and
/// heals by hand) must not leave the connection waiting out that hour.
#[test]
fn healing_a_partition_releases_the_stalled_bytes_immediately() {
    const PAYLOAD: &[u8] = b"payload-held-by-the-partition";
    const CUT: Duration = Duration::from_hours(1);

    let mut sim =
        SimWorld::new_with_network_config_and_seed(NetworkConfiguration::fast_local(), 20_240_183);
    let client_ip = ip(1);
    let server_ip = ip(2);

    let server_provider = sim.network_provider(server_ip);
    let listener = drive(&mut sim, server_provider.bind("10.0.1.2:8080")).expect("bind");
    let client_provider = sim.network_provider(client_ip);
    let mut client = drive(&mut sim, client_provider.connect("10.0.1.2:8080")).expect("connect");
    let (mut server, _) = drive(&mut sim, listener.accept()).expect("accept");
    sim.run_until_empty();

    sim.partition_pair(client_ip, server_ip, CUT);
    assert_eq!(
        poll_write_once(&mut client, PAYLOAD).map(|result| result.expect("payload is accepted")),
        Poll::Ready(PAYLOAD.len())
    );
    assert!(sim.step(), "the engine drains the queued send");
    let stalled_at = sim.current_time();
    let mut byte = [0_u8; 1];
    assert!(poll_read_once(&mut server, &mut byte).is_pending());

    sim.restore_partition(client_ip, server_ip);
    let mut received = vec![0_u8; PAYLOAD.len()];
    drive(&mut sim, server.read_exact(&mut received)).expect("the payload arrives");

    assert_eq!(received, PAYLOAD);
    assert!(
        sim.current_time().saturating_sub(stalled_at) < Duration::from_secs(1),
        "healing re-drives the stream instead of waiting out the cut"
    );
}

/// A send-wide partition stalls the stream too, and its own clear event
/// resumes it.
#[test]
fn a_send_partition_stalls_the_stream_until_it_clears() {
    const PAYLOAD: &[u8] = b"payload-held-by-the-send-cut";
    const CUT: Duration = Duration::from_millis(200);

    let mut sim =
        SimWorld::new_with_network_config_and_seed(NetworkConfiguration::fast_local(), 20_240_183);
    let client_ip = ip(1);
    let server_ip = ip(2);

    let server_provider = sim.network_provider(server_ip);
    let listener = drive(&mut sim, server_provider.bind("10.0.1.2:8080")).expect("bind");
    let client_provider = sim.network_provider(client_ip);
    let mut client = drive(&mut sim, client_provider.connect("10.0.1.2:8080")).expect("connect");
    let (mut server, _) = drive(&mut sim, listener.accept()).expect("accept");
    sim.run_until_empty();

    sim.partition_send_from(client_ip, CUT);
    assert_eq!(
        poll_write_once(&mut client, PAYLOAD).map(|result| result.expect("payload is accepted")),
        Poll::Ready(PAYLOAD.len())
    );
    assert!(sim.step(), "the engine drains the queued send");
    let mut byte = [0_u8; 1];
    assert!(poll_read_once(&mut server, &mut byte).is_pending());

    let mut received = vec![0_u8; PAYLOAD.len()];
    drive(&mut sim, server.read_exact(&mut received)).expect("the payload arrives once it clears");

    assert_eq!(received, PAYLOAD);
    assert!(
        sim.current_time() >= CUT,
        "the bytes waited for the send partition to clear"
    );
}
