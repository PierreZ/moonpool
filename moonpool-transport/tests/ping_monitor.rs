//! Tests for the connection ping monitor (connectionMonitor pattern).
//!
//! FDB Reference: connectionMonitor (FlowTransport.actor.cpp:616-699)
//!
//! Tests verify that:
//! - Ping/pong messages flow correctly between connected peers
//! - Ping timeout triggers connection failure when remote stops responding
//! - Metrics (RTT, pong count, timeout count) are updated correctly

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use moonpool_sim::{
    NetworkConfiguration, NetworkProvider, Providers, SimProviders, SimWorld, TcpListenerTrait,
    TimeProvider,
};
use moonpool_transport::{HEADER_SIZE, Peer, PeerConfig, PeerMetrics, try_deserialize_packet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Create a local tokio runtime for tests.
fn local_runtime() -> tokio::runtime::LocalRuntime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime")
}

/// Test that ping timeout triggers connection failure when remote stops responding.
///
/// This is the core test for the connectionMonitor pattern:
/// 1. Establish connection with ping monitoring
/// 2. Accept connection on server but never respond to pings
/// 3. Verify ping timeout triggers and peer reports failure
#[test]
fn test_ping_timeout_triggers_reconnection() {
    local_runtime().block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let providers = SimProviders::new(sim.downgrade(), 42);

        // Shared metrics result
        let result: Rc<RefCell<Option<PeerMetrics>>> = Rc::new(RefCell::new(None));
        let result_clone = result.clone();

        let handle = tokio::task::spawn_local(async move {
            // Server: bind and accept but NEVER respond to pings
            let listener = providers.network().bind("server").await.expect("bind");

            // Spawn server that accepts and reads data but never writes back
            tokio::task::spawn_local({
                let providers = providers.clone();
                async move {
                    let (mut stream, _addr) = listener.accept().await.expect("accept");
                    let mut buf = vec![0u8; 4096];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {} // Received data (pings) - silently drop
                        }
                    }
                    let _ = providers;
                }
            });

            // Client: peer with aggressive ping timing for fast test
            let peer_config = PeerConfig::new(
                100,                        // max_queue_size
                Duration::from_millis(500), // connection_timeout
                Duration::from_millis(50),  // initial_reconnect_delay
                Duration::from_secs(1),     // max_reconnect_delay
                Some(3),                    // max_connection_failures
            )
            .with_ping(
                Duration::from_millis(100), // ping_interval
                Duration::from_millis(200), // ping_timeout
            );
            let peer = Rc::new(RefCell::new(Peer::new(
                providers.clone(),
                "server".to_string(),
                peer_config,
            )));

            // Wait for connection + pings + timeout by using sim time sleeps
            let time = providers.time().clone();
            time.sleep(Duration::from_millis(50)).await.ok();
            time.sleep(Duration::from_millis(500)).await.ok();

            let metrics = peer.borrow().metrics();
            *result_clone.borrow_mut() = Some(metrics);
            peer.borrow_mut().close().await;
        });

        // Drive simulation: step ONE event then yield (matches orchestrator pattern)
        while !handle.is_finished() {
            if sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked");

        let metrics = result.borrow().clone().expect("metrics should be set");
        assert!(
            metrics.pings_sent > 0,
            "Expected pings to be sent, got pings_sent={}",
            metrics.pings_sent
        );
        assert!(
            metrics.ping_timeouts > 0,
            "Expected ping timeouts, got ping_timeouts={}",
            metrics.ping_timeouts
        );
        assert_eq!(
            metrics.pongs_received, 0,
            "Expected no pongs from unresponsive server"
        );
    });
}

/// Test that ping/pong flows correctly between two peers and RTT is recorded.
#[test]
fn test_ping_pong_healthy_connection() {
    local_runtime().block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let providers = SimProviders::new(sim.downgrade(), 42);

        let result: Rc<RefCell<Option<PeerMetrics>>> = Rc::new(RefCell::new(None));
        let result_clone = result.clone();

        let handle = tokio::task::spawn_local(async move {
            // Server: bind and accept, respond to pings with pongs
            let listener = providers.network().bind("server").await.expect("bind");

            tokio::task::spawn_local({
                let providers = providers.clone();
                async move {
                    let (mut stream, _addr) = listener.accept().await.expect("accept");
                    let mut buf = vec![0u8; 4096];
                    let mut read_buffer = Vec::new();

                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                read_buffer.extend_from_slice(&buf[..n]);
                                // Parse packets and respond to pings
                                while read_buffer.len() >= HEADER_SIZE {
                                    match try_deserialize_packet(&read_buffer) {
                                        Ok(Some((token, payload, consumed))) => {
                                            read_buffer.drain(..consumed);
                                            let ping_uid =
                                                moonpool_transport::WellKnownToken::Ping.uid();
                                            if token == ping_uid
                                                && !payload.is_empty()
                                                && payload[0] == 0
                                            {
                                                // Create pong: type=1 + echoed timestamp
                                                let mut pong = vec![1u8];
                                                if payload.len() >= 9 {
                                                    pong.extend_from_slice(&payload[1..9]);
                                                }
                                                if let Ok(packet) =
                                                    moonpool_transport::serialize_packet(
                                                        ping_uid, &pong,
                                                    )
                                                {
                                                    let _ = stream.write_all(&packet).await;
                                                }
                                            }
                                        }
                                        Ok(None) => break,
                                        Err(_) => {
                                            read_buffer.clear();
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let _ = providers;
                }
            });

            // Client: peer with ping enabled
            let peer_config = PeerConfig::default()
                .with_ping(Duration::from_millis(100), Duration::from_millis(500));
            let peer = Rc::new(RefCell::new(Peer::new(
                providers.clone(),
                "server".to_string(),
                peer_config,
            )));

            // Wait long enough for connection + multiple ping/pong cycles
            let time = providers.time().clone();
            time.sleep(Duration::from_millis(500)).await.ok();

            let metrics = peer.borrow().metrics();
            *result_clone.borrow_mut() = Some(metrics);
            peer.borrow_mut().close().await;
        });

        while !handle.is_finished() {
            if sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked");

        let metrics = result.borrow().clone().expect("metrics should be set");
        assert!(
            metrics.pings_sent > 0,
            "Expected pings to be sent, got pings_sent={}",
            metrics.pings_sent
        );
        assert!(
            metrics.pongs_received > 0,
            "Expected pongs to be received, got pongs_received={}",
            metrics.pongs_received
        );
        assert!(
            metrics.last_ping_rtt.is_some(),
            "Expected RTT to be recorded"
        );
        assert!(
            metrics.last_pong_received_at.is_some(),
            "Expected last_pong_received_at to be set"
        );
        assert_eq!(
            metrics.ping_timeouts, 0,
            "Expected no ping timeouts on healthy connection"
        );
    });
}

/// Test that ping is disabled when interval is zero (default).
#[test]
fn test_ping_disabled_by_default() {
    local_runtime().block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let providers = SimProviders::new(sim.downgrade(), 42);

        let result: Rc<RefCell<Option<PeerMetrics>>> = Rc::new(RefCell::new(None));
        let result_clone = result.clone();

        let handle = tokio::task::spawn_local(async move {
            let listener = providers.network().bind("server").await.expect("bind");

            tokio::task::spawn_local({
                let providers = providers.clone();
                async move {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut buf = vec![0u8; 4096];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                    let _ = providers;
                }
            });

            // Default config has ping_interval = 0 (disabled)
            let peer = Rc::new(RefCell::new(Peer::new_with_defaults(
                providers.clone(),
                "server".to_string(),
            )));

            let time = providers.time().clone();
            time.sleep(Duration::from_millis(500)).await.ok();

            let metrics = peer.borrow().metrics();
            *result_clone.borrow_mut() = Some(metrics);
            peer.borrow_mut().close().await;
        });

        while !handle.is_finished() {
            if sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked");

        let metrics = result.borrow().clone().expect("metrics should be set");
        assert_eq!(metrics.pings_sent, 0, "No pings when disabled");
        assert_eq!(metrics.ping_timeouts, 0, "No timeouts when disabled");
    });
}

/// Test that connection_monitor queues pings but they fail when server unreachable.
///
/// The monitor queues pings to trigger connection establishment, but since
/// there's no server, the connection will fail. Pings are queued (counted
/// as sent) but no pongs are received.
#[test]
fn test_ping_metrics_no_connection() {
    local_runtime().block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let providers = SimProviders::new(sim.downgrade(), 42);

        let result: Rc<RefCell<Option<PeerMetrics>>> = Rc::new(RefCell::new(None));
        let result_clone = result.clone();

        let handle = tokio::task::spawn_local(async move {
            // No server - peer will fail to connect
            let peer_config = PeerConfig::new(
                100,
                Duration::from_millis(200),
                Duration::from_millis(50),
                Duration::from_secs(1),
                Some(2), // Fail fast
            )
            .with_ping(Duration::from_millis(100), Duration::from_millis(200));

            let peer = Rc::new(RefCell::new(Peer::new(
                providers.clone(),
                "nonexistent".to_string(),
                peer_config,
            )));

            let time = providers.time().clone();
            time.sleep(Duration::from_millis(500)).await.ok();

            let metrics = peer.borrow().metrics();
            *result_clone.borrow_mut() = Some(metrics);
            peer.borrow_mut().close().await;
        });

        while !handle.is_finished() {
            if sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked");

        let metrics = result.borrow().clone().expect("metrics should be set");
        // Pings are queued by connection_monitor (counted as sent),
        // but connection establishment fails so no pongs arrive.
        assert_eq!(
            metrics.pongs_received, 0,
            "Should not receive pongs when server unreachable"
        );
    });
}
