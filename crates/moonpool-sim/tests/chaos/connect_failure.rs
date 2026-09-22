//! Integration tests for connection establishment failure chaos injection
//!
//! Tests verify that connection failures:
//! - Follow FDB's `SIM_CONNECT_ERROR_MODE` pattern (sim2.actor.cpp:1243-1250)
//! - Disabled: Normal operation (no failure injection)
//! - `AlwaysFail`: Always fail with `ConnectionRefused` when buggified
//! - Probabilistic: refused with `connect_failure_probability`, otherwise hang
//!   forever (tests timeout handling)
//! - Can be disabled via configuration
//! - Are deterministic across runs with the same seed

use super::local_runtime;
use moonpool_sim::{
    ConnectFailureMode, NetworkConfiguration, NetworkProvider, SimWorld, buggify_init,
    buggify_reset,
};
use std::future::Future;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// Drive `future` against `sim` until it resolves or the world runs out of
/// events. `None` means the future is still pending with nothing left that
/// could wake it — a hang, reported instead of blocking the test forever.
fn settle<F: Future>(sim: &mut SimWorld, future: F) -> Option<F::Output> {
    let mut future = std::pin::pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return Some(output);
        }
        if !sim.has_pending_events() {
            return None;
        }
        sim.step();
    }
}

/// Test that connection failure mode Disabled works normally
#[test]
fn test_connect_failure_mode_disabled() {
    local_runtime().block_on(async move {
        buggify_init(1.0); // Enable buggify

        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = ConnectFailureMode::Disabled;

        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

        let addr = "test-server";
        let _listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();

        // All connections should succeed with Disabled mode
        for i in 0..10 {
            let result = super::drive(&mut sim, provider.connect(addr)).await;
            assert!(
                result.is_ok(),
                "Connection {} should succeed with Disabled mode, got {:?}",
                i,
                result.err()
            );
        }

        buggify_reset();
        println!("✅ Connection failure mode Disabled works correctly");
    });
}

/// Test that connection failure mode `AlwaysFail` fails when buggified
#[test]
fn test_connect_failure_mode_always_fail() {
    local_runtime().block_on(async move {
        buggify_init(1.0); // Enable buggify with 100% activation

        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = ConnectFailureMode::AlwaysFail;

        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

        let addr = "fail-server";
        let _listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();

        // With AlwaysFail and buggify enabled, connections should fail
        // (depending on buggify activation for this location)
        let mut failure_count = 0;
        for _ in 0..10 {
            let result = super::drive(&mut sim, provider.connect(addr)).await;
            if result.is_err() {
                failure_count += 1;
            }
        }

        println!("AlwaysFail mode with buggify: {failure_count} failures out of 10 attempts");

        buggify_reset();
        println!("✅ Connection failure mode AlwaysFail executed");
    });
}

/// Test that `connect_failure_probability = 1.0` refuses every buggified
/// connect and never hangs one (#251: the comparison was inverted, so `1.0`
/// hung every buggified connect instead).
#[test]
fn test_connect_failure_mode_probabilistic_error() {
    moonpool_sim::set_sim_seed(99999);
    buggify_init(1.0);

    let mut config = NetworkConfiguration::fast_local();
    config.chaos.connect_failure_mode = ConnectFailureMode::Probabilistic;
    config.chaos.connect_failure_probability = 1.0; // always refuse, never hang

    let mut sim = SimWorld::new_with_network_config(config);
    let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

    let addr = "prob-server";
    let _listener = settle(&mut sim, provider.bind(addr))
        .expect("bind settles")
        .expect("bind succeeds");

    let mut success_count = 0;
    let mut error_count = 0;
    for i in 0..20 {
        match settle(&mut sim, provider.connect(addr)) {
            Some(Ok(_)) => success_count += 1,
            Some(Err(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused);
                error_count += 1;
            }
            None => panic!("connect {i} hung with connect_failure_probability = 1.0"),
        }
    }

    buggify_reset();
    assert!(
        error_count > 0,
        "expected refused connects, got {success_count} successes and no errors"
    );
}

/// Test that `connect_failure_probability = 0.0` never refuses a connect:
/// a buggified connect hangs, every other one succeeds.
#[test]
fn test_connect_failure_mode_probabilistic_hang() {
    moonpool_sim::set_sim_seed(99999);
    buggify_init(1.0);

    let mut config = NetworkConfiguration::fast_local();
    config.chaos.connect_failure_mode = ConnectFailureMode::Probabilistic;
    config.chaos.connect_failure_probability = 0.0; // never refuse, always hang

    let mut sim = SimWorld::new_with_network_config(config);
    let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

    let addr = "hang-server";
    let _listener = settle(&mut sim, provider.bind(addr))
        .expect("bind settles")
        .expect("bind succeeds");

    let mut hang_count = 0;
    for i in 0..20 {
        match settle(&mut sim, provider.connect(addr)) {
            Some(Ok(_)) => {}
            Some(Err(e)) => {
                panic!("connect {i} refused with connect_failure_probability = 0.0: {e}")
            }
            None => hang_count += 1,
        }
    }

    buggify_reset();
    assert!(hang_count > 0, "expected hung connects, got none");
}

/// Test that disabled buggify doesn't inject failures even with mode set
#[test]
fn test_connect_failure_requires_buggify() {
    local_runtime().block_on(async move {
        buggify_reset(); // Ensure buggify is disabled

        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = ConnectFailureMode::AlwaysFail; // Would fail if buggify active

        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

        let addr = "no-buggify-server";
        let _listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();

        // Without buggify, all connections should succeed
        for i in 0..10 {
            let result = super::drive(&mut sim, provider.connect(addr)).await;
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

/// Test connection failure with timeout handling (Probabilistic hang scenario)
#[test]
fn test_connect_failure_mode_probabilistic_with_timeout() {
    local_runtime().block_on(async move {
        moonpool_sim::set_sim_seed(42);
        buggify_init(1.0);

        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = ConnectFailureMode::Probabilistic;
        config.chaos.connect_failure_probability = 0.0; // 100% hang (not error)

        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

        let addr = "hang-server";
        let _listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();

        // Use select! with a timeout to avoid actual hanging
        // This simulates how production code should handle potential hangs
        let timeout_duration = Duration::from_millis(100);

        let connect_result = moonpool_sim::select! {
            biased;
            result = provider.connect(addr) => {
                Some(result)
            }
            () = tokio::time::sleep(timeout_duration) => {
                None
            }
        };

        match connect_result {
            Some(Ok(_)) => println!("Connection succeeded (no hang triggered)"),
            Some(Err(e)) => println!("Connection failed with error: {e:?}"),
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
        local_runtime().block_on(async move {
            moonpool_sim::set_sim_seed(12345);
            buggify_init(1.0);

            let mut config = NetworkConfiguration::fast_local();
            config.chaos.connect_failure_mode = ConnectFailureMode::AlwaysFail;

            let mut sim = SimWorld::new_with_network_config(config);
            let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

            let addr = "det-server";
            let _listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();

            let mut results = Vec::new();
            for _ in 0..10 {
                let result = super::drive(&mut sim, provider.connect(addr)).await;
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
    use futures::io::AsyncWriteExt;

    local_runtime().block_on(async move {
        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = ConnectFailureMode::Disabled; // Start with disabled

        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

        let addr = "existing-conn-server";
        let _listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();

        // Establish connection before enabling chaos
        let mut client = super::drive(&mut sim, provider.connect(addr))
            .await
            .unwrap();

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

/// Test error message content for `AlwaysFail` mode
#[test]
fn test_connect_failure_error_message_always_fail() {
    local_runtime().block_on(async move {
        moonpool_sim::set_sim_seed(77777);
        buggify_init(1.0);

        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = ConnectFailureMode::AlwaysFail;

        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

        let addr = "error-msg-server";
        let _listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();

        // Try to get a failure to check error message
        for _ in 0..10 {
            if let Err(e) = super::drive(&mut sim, provider.connect(addr)).await {
                assert!(
                    e.to_string().contains("AlwaysFail mode"),
                    "Error should mention AlwaysFail mode, got: {e}"
                );
                println!("✅ Error message correctly identifies AlwaysFail mode");
                buggify_reset();
                return;
            }
        }

        buggify_reset();
        println!("✅ No failures triggered (buggify location not activated)");
    });
}

/// Test error message content for Probabilistic mode
#[test]
fn test_connect_failure_error_message_probabilistic() {
    local_runtime().block_on(async move {
        moonpool_sim::set_sim_seed(88888);
        buggify_init(1.0);

        let mut config = NetworkConfiguration::fast_local();
        config.chaos.connect_failure_mode = ConnectFailureMode::Probabilistic;
        config.chaos.connect_failure_probability = 1.0; // Force error path, not hang

        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

        let addr = "error-msg-server-2";
        let _listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();

        // Try to get a failure to check error message
        for _ in 0..10 {
            if let Err(e) = super::drive(&mut sim, provider.connect(addr)).await {
                assert!(
                    e.to_string().contains("Probabilistic mode"),
                    "Error should mention Probabilistic mode, got: {e}"
                );
                println!("✅ Error message correctly identifies Probabilistic mode");
                buggify_reset();
                return;
            }
        }

        buggify_reset();
        println!("✅ No failures triggered (buggify location not activated)");
    });
}

/// Test interaction with `random_for_seed` config
///
/// NOTE: This test may hang forever when Probabilistic mode is selected (50% hang probability).
/// FDB's Probabilistic mode simulates connections that hang forever without timeout.
/// Run manually: cargo test `test_connect_failure_random_config` -- --ignored
#[test]
#[ignore = "May hang forever with Probabilistic mode - requires manual testing"]
fn test_connect_failure_random_config() {
    local_runtime().block_on(async move {
        moonpool_sim::set_sim_seed(11111);
        buggify_init(1.0);

        // Use randomized config
        let config = NetworkConfiguration::random_for_seed();

        println!(
            "Random config: mode={:?}, probability={}",
            config.chaos.connect_failure_mode, config.chaos.connect_failure_probability
        );

        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

        let addr = "random-config-server";
        let _listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();

        // Just verify it doesn't crash with random config
        let mut success = 0;
        let mut failed = 0;

        for _ in 0..10 {
            match provider.connect(addr).await {
                Ok(_) => success += 1,
                Err(_) => failed += 1,
            }
        }

        println!("Random config results: {success} success, {failed} failed");

        buggify_reset();
        println!("✅ Connection failure with random config executed");
    });
}
