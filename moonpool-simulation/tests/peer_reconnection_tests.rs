//! Integration tests for Peer reconnection logic with TimeProvider.

use moonpool_simulation::{
    Peer, PeerConfig, SimWorld, TimeProvider, TokioNetworkProvider, TokioTimeProvider,
};
use std::time::Duration;

/// Test that Peer can be created with both provider combinations
#[tokio::test]
async fn test_peer_provider_combinations() {
    // Simulation providers
    let sim = SimWorld::new();
    let sim_network = sim.network_provider();
    let sim_time = sim.time_provider();
    let _sim_peer = Peer::new_with_defaults(sim_network, sim_time, "test:8080".to_string());

    // Real providers
    let tokio_network = TokioNetworkProvider::new();
    let tokio_time = TokioTimeProvider::new();
    let _tokio_peer = Peer::new_with_defaults(tokio_network, tokio_time, "test:8080".to_string());

    // Mixed providers (real network, simulated time)
    let tokio_network = TokioNetworkProvider::new();
    let sim_time = sim.time_provider();
    let _mixed_peer = Peer::new_with_defaults(tokio_network, sim_time, "test:8080".to_string());
}

/// Test basic Peer creation with different provider combinations
#[tokio::test]
async fn test_peer_creation_combinations() {
    let sim = SimWorld::new();
    let sim_network = sim.network_provider();
    let sim_time = sim.time_provider();

    // Test with simulation providers
    let peer1 = Peer::new_with_defaults(sim_network.clone(), sim_time.clone(), "test:8080".to_string());
    assert_eq!(peer1.destination(), "test:8080");
    assert!(!peer1.is_connected());

    // Test with real providers
    let tokio_network = TokioNetworkProvider::new();
    let tokio_time = TokioTimeProvider::new();
    let peer2 = Peer::new_with_defaults(tokio_network, tokio_time, "test:8081".to_string());
    assert_eq!(peer2.destination(), "test:8081");
    assert!(!peer2.is_connected());

    // Test mixed providers
    let tokio_network2 = TokioNetworkProvider::new();
    let peer3 = Peer::new_with_defaults(tokio_network2, sim_time, "test:8082".to_string());
    assert_eq!(peer3.destination(), "test:8082");
    assert!(!peer3.is_connected());
}

/// Test TimeProvider trait methods work correctly  
#[tokio::test]
async fn test_time_provider_methods() {
    // Test TokioTimeProvider
    let tokio_time = TokioTimeProvider::new();
    let now1 = tokio_time.now();
    assert!(now1.elapsed() < Duration::from_millis(100));
    
    // Quick sleep test
    let start = std::time::Instant::now();
    let result = tokio_time.sleep(Duration::from_millis(1)).await;
    let elapsed = start.elapsed();
    assert!(result.is_ok());
    assert!(elapsed >= Duration::from_millis(1));
    
    // Test SimTimeProvider
    let sim = SimWorld::new();
    let sim_time = sim.time_provider();
    let now2 = sim_time.now();
    assert!(now2.elapsed() < Duration::from_millis(100));
    
    // Timeout test - immediate completion
    let result = sim_time.timeout(Duration::from_millis(100), async { 42 }).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), Ok(42));
}

/// Test basic Peer configuration and creation
#[tokio::test]
async fn test_peer_configuration() {
    let sim = SimWorld::new();
    let network = sim.network_provider();
    let time = sim.time_provider();

    let config = PeerConfig {
        initial_reconnect_delay: Duration::from_millis(100),
        max_reconnect_delay: Duration::from_millis(1000),
        max_queue_size: 10,
        connection_timeout: Duration::from_millis(500),
        max_connection_failures: Some(5),
    };

    let peer = Peer::new(network, time, "test:8080".to_string(), config);

    // Verify initial state
    assert_eq!(peer.destination(), "test:8080");
    assert!(!peer.is_connected());
    assert_eq!(peer.queue_size(), 0);
    
    // Verify metrics initialization
    let metrics = peer.metrics();
    assert_eq!(metrics.connection_attempts, 0);
    assert_eq!(metrics.connections_established, 0);
    assert_eq!(metrics.connection_failures, 0);
    assert!(!metrics.is_connected);
}

/// Test Peer configuration presets
#[test]
fn test_peer_config_presets() {
    let local_config = PeerConfig::local_network();
    assert_eq!(local_config.initial_reconnect_delay, Duration::from_millis(10));
    assert_eq!(local_config.max_reconnect_delay, Duration::from_secs(1));
    assert_eq!(local_config.connection_timeout, Duration::from_millis(500));
    assert_eq!(local_config.max_connection_failures, Some(10));
    
    let wan_config = PeerConfig::wan_network();
    assert_eq!(wan_config.initial_reconnect_delay, Duration::from_millis(500));
    assert_eq!(wan_config.max_reconnect_delay, Duration::from_secs(60));
    assert_eq!(wan_config.connection_timeout, Duration::from_secs(30));
    assert_eq!(wan_config.max_connection_failures, None);
    
    let default_config = PeerConfig::default();
    assert_eq!(default_config.initial_reconnect_delay, Duration::from_millis(100));
    assert_eq!(default_config.max_reconnect_delay, Duration::from_secs(30));
    assert_eq!(default_config.connection_timeout, Duration::from_secs(5));
    assert_eq!(default_config.max_connection_failures, None);
}
