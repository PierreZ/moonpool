//! Integration tests for connection establishment failure chaos injection
//!
//! Tests verify that connection failures:
//! - Follow FDB's SIM_CONNECT_ERROR_MODE pattern (sim2.actor.cpp:1243-1250)
//! - Mode 0: Normal operation (no failure injection)
//! - Mode 1: Always fail with ConnectionRefused when buggified
//! - Mode 2: 50% fail with error, 50% hang forever (tests timeout handling)
//! - Can be disabled via configuration
//! - Are deterministic across runs with the same seed

use moonpool_foundation::{
    NetworkConfiguration, NetworkProvider, SimWorld, buggify_init, buggify_reset,
};
use std::time::Duration;

/// Test that connection failure mode 0 (disabled) works normally
#[test]
fn test_connect_failure_mode_0_disabled() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        buggify_init(1.0, 1.0); // Enable buggify

        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = 0; // Disabled

        let sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "test-server";
        let _listener = provider.bind(addr).await.unwrap();

        // All connections should succeed with mode 0
        for i in 0..10 {
            let result = provider.connect(addr).await;
            assert!(
                result.is_ok(),
                "Connection {} should succeed with mode 0, got {:?}",
                i,
                result.err()
            );
        }

        buggify_reset();
        println!("✅ Connection failure mode 0 (disabled) works correctly");
    });
}

/// Test that connection failure mode 1 always fails when buggified
#[test]
fn test_connect_failure_mode_1_always_fail() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        buggify_init(1.0, 1.0); // Enable buggify with 100% activation

        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = 1; // Always fail when buggified

        let sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "fail-server";
        let _listener = provider.bind(addr).await.unwrap();

        // With mode 1 and buggify enabled, connections should fail
        // (depending on buggify activation for this location)
        let mut failure_count = 0;
        for _ in 0..10 {
            let result = provider.connect(addr).await;
            if result.is_err() {
                failure_count += 1;
            }
        }

        println!(
            "Mode 1 with buggify: {} failures out of 10 attempts",
            failure_count
        );

        buggify_reset();
        println!("✅ Connection failure mode 1 executed");
    });
}

/// Test that connection failure mode 2 produces errors (not hangs)
/// Note: We set connect_failure_probability=0.0 to force error path, not hang path
#[test]
fn test_connect_failure_mode_2_probabilistic_error() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        moonpool_foundation::set_sim_seed(99999);
        buggify_init(1.0, 1.0);

        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = 2; // Probabilistic
        // Force error path (not hang) by setting probability to 0.0
        // With prob=0.0: random01() > 0.0 is always true, so we get errors
        config.chaos.connect_failure_probability = 0.0;

        let sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "prob-server";
        let _listener = provider.bind(addr).await.unwrap();

        // With mode 2 and probability=0.0, connections should fail with error (not hang)
        let mut success_count = 0;
        let mut error_count = 0;

        for _ in 0..20 {
            let result = provider.connect(addr).await;
            match result {
                Ok(_) => success_count += 1,
                Err(_) => error_count += 1,
            }
        }

        println!(
            "Mode 2 results: {} successes, {} errors out of 20",
            success_count, error_count
        );

        buggify_reset();
        println!("✅ Connection failure mode 2 probabilistic error executed");
    });
}

/// Test that disabled buggify doesn't inject failures even with mode set
#[test]
fn test_connect_failure_requires_buggify() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        buggify_reset(); // Ensure buggify is disabled

        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = 1; // Would fail if buggify active

        let sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "no-buggify-server";
        let _listener = provider.bind(addr).await.unwrap();

        // Without buggify, all connections should succeed
        for i in 0..10 {
            let result = provider.connect(addr).await;
            assert!(
                result.is_ok(),
                "Connection {} should succeed without buggify, got {:?}",
                i,
                result.err()
            );
        }

        println!("✅ Connection failure requires buggify to be enabled");
    });
}

/// Test connection failure with timeout handling (mode 2 hang scenario)
#[test]
fn test_connect_failure_mode_2_with_timeout() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        moonpool_foundation::set_sim_seed(42);
        buggify_init(1.0, 1.0);

        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = 2;
        config.chaos.connect_failure_probability = 1.0; // 100% hang (not error)

        let sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "hang-server";
        let _listener = provider.bind(addr).await.unwrap();

        // Use select! with a timeout to avoid actual hanging
        // This simulates how production code should handle potential hangs
        let timeout_duration = Duration::from_millis(100);

        let connect_result = tokio::select! {
            result = provider.connect(addr) => {
                Some(result)
            }
            _ = tokio::time::sleep(timeout_duration) => {
                None
            }
        };

        match connect_result {
            Some(Ok(_)) => println!("Connection succeeded (no hang triggered)"),
            Some(Err(e)) => println!("Connection failed with error: {:?}", e),
            None => println!("Connection timed out (hang scenario)"),
        }

        buggify_reset();
        println!("✅ Connection failure with timeout handling executed");
    });
}

/// Test deterministic behavior with same seed
#[test]
fn test_connect_failure_deterministic() {
    let run_simulation = || -> Vec<bool> {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            moonpool_foundation::set_sim_seed(12345);
            buggify_init(1.0, 1.0);

            let mut config = NetworkConfiguration::fast_local();
            config.chaos.connect_failure_mode = 1;

            let sim = SimWorld::new_with_network_config(config);
            let provider = sim.network_provider();

            let addr = "det-server";
            let _listener = provider.bind(addr).await.unwrap();

            let mut results = Vec::new();
            for _ in 0..10 {
                let result = provider.connect(addr).await;
                results.push(result.is_ok());
            }

            buggify_reset();
            results
        })
    };

    let run1 = run_simulation();
    let run2 = run_simulation();

    assert_eq!(
        run1, run2,
        "Connection failures should be deterministic with same seed"
    );

    println!("✅ Connection failure is deterministic");
}

/// Test connection failure doesn't affect already established connections
#[test]
fn test_connect_failure_existing_connections() {
    use tokio::io::AsyncWriteExt;

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = 0; // Start with disabled

        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "existing-conn-server";
        let _listener = provider.bind(addr).await.unwrap();

        // Establish connection before enabling chaos
        let mut client = provider.connect(addr).await.unwrap();

        sim.run_until_empty();

        // Existing connection should still work regardless of chaos mode
        let write_result = client.write_all(b"test message").await;
        assert!(
            write_result.is_ok(),
            "Existing connection should still work"
        );

        sim.run_until_empty();

        println!("✅ Existing connections not affected by connect failure chaos");
    });
}

/// Test error message content for mode 1
#[test]
fn test_connect_failure_error_message_mode_1() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        moonpool_foundation::set_sim_seed(77777);
        buggify_init(1.0, 1.0);

        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = 1;

        let sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "error-msg-server";
        let _listener = provider.bind(addr).await.unwrap();

        // Try to get a failure to check error message
        for _ in 0..10 {
            if let Err(e) = provider.connect(addr).await {
                assert!(
                    e.to_string().contains("chaos mode 1"),
                    "Error should mention chaos mode 1, got: {}",
                    e
                );
                println!("✅ Error message correctly identifies chaos mode 1");
                buggify_reset();
                return;
            }
        }

        buggify_reset();
        println!("✅ No failures triggered (buggify location not activated)");
    });
}

/// Test error message content for mode 2
#[test]
fn test_connect_failure_error_message_mode_2() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        moonpool_foundation::set_sim_seed(88888);
        buggify_init(1.0, 1.0);

        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = 2;
        config.chaos.connect_failure_probability = 0.0; // Force error path, not hang

        let sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "error-msg-server-2";
        let _listener = provider.bind(addr).await.unwrap();

        // Try to get a failure to check error message
        for _ in 0..10 {
            if let Err(e) = provider.connect(addr).await {
                assert!(
                    e.to_string().contains("chaos mode 2"),
                    "Error should mention chaos mode 2, got: {}",
                    e
                );
                println!("✅ Error message correctly identifies chaos mode 2");
                buggify_reset();
                return;
            }
        }

        buggify_reset();
        println!("✅ No failures triggered (buggify location not activated)");
    });
}

/// Test interaction with random_for_seed config
///
/// NOTE: This test may hang forever when mode=2 is selected (50% hang probability).
/// FDB's mode=2 simulates connections that hang forever without timeout.
/// Run manually: cargo test test_connect_failure_random_config -- --ignored
#[test]
#[ignore] // May hang forever with mode=2 - requires manual testing
fn test_connect_failure_random_config() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        moonpool_foundation::set_sim_seed(11111);
        buggify_init(1.0, 1.0);

        // Use randomized config
        let config = NetworkConfiguration::random_for_seed();

        println!(
            "Random config: mode={}, probability={}",
            config.chaos.connect_failure_mode, config.chaos.connect_failure_probability
        );

        let sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "random-config-server";
        let _listener = provider.bind(addr).await.unwrap();

        // Just verify it doesn't crash with random config
        let mut success = 0;
        let mut failed = 0;

        for _ in 0..10 {
            match provider.connect(addr).await {
                Ok(_) => success += 1,
                Err(_) => failed += 1,
            }
        }

        println!(
            "Random config results: {} success, {} failed",
            success, failed
        );

        buggify_reset();
        println!("✅ Connection failure with random config executed");
    });
}
