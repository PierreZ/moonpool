use moonpool_simulation::{AssertionStats, SimWorld, always_assert, sometimes_assert};
use std::sync::{Arc, Barrier};
use std::thread;

#[test]
fn test_always_assert_success() {
    let sim = SimWorld::new_with_seed(42);

    let value = 42;
    always_assert!(value_check, value == 42, "Value should be 42");

    // always_assert! no longer tracks successful assertions
    let results = sim.assertion_results();
    assert!(results.is_empty(), "always_assert! should not be tracked when successful");
}

#[test]
#[should_panic(expected = "Always assertion 'failing_check' failed (seed: 42): This should fail")]
fn test_always_assert_failure() {
    let _sim = SimWorld::new_with_seed(42);

    let value = 42;
    always_assert!(failing_check, value == 0, "This should fail");
}

#[test]
fn test_sometimes_assert_tracking() {
    let sim = SimWorld::new_with_seed(42);

    // Test with multiple assertions with different outcomes
    sometimes_assert!(fast_operation, true, "Operation should be fast");
    sometimes_assert!(fast_operation, false, "Operation should be fast");
    sometimes_assert!(fast_operation, true, "Operation should be fast");
    sometimes_assert!(fast_operation, true, "Operation should be fast");

    let results = sim.assertion_results();
    let stats = &results["fast_operation"];
    assert_eq!(stats.total_checks, 4);
    assert_eq!(stats.successes, 3);
    assert_eq!(stats.success_rate(), 75.0);
}

#[test]
fn test_multiple_assertion_types() {
    let sim = SimWorld::new_with_seed(123);

    // Mix of different assertion types
    always_assert!(system_valid, true, "System should be valid");
    sometimes_assert!(performance_good, true, "Performance should be good");
    sometimes_assert!(performance_good, false, "Performance should be good");
    always_assert!(invariant_holds, 1 + 1 == 2, "Math should work");

    let results = sim.assertion_results();

    // Only sometimes_assert! calls are tracked now
    assert_eq!(results.len(), 1);
    
    // Check performance_good assertions (only sometimes_assert! tracked)
    let perf_stats = &results["performance_good"];
    assert_eq!(perf_stats.total_checks, 2);
    assert_eq!(perf_stats.successes, 1);
    assert_eq!(perf_stats.success_rate(), 50.0);

    // always_assert! calls are no longer tracked
    assert!(!results.contains_key("system_valid"));
    assert!(!results.contains_key("invariant_holds"));
}

#[test]
fn test_assertion_reset_between_simulations() {
    // First simulation
    {
        let sim1 = SimWorld::new_with_seed(42);
        sometimes_assert!(test_metric, true, "Test metric");
        sometimes_assert!(test_metric, false, "Test metric");

        let results1 = sim1.assertion_results();
        assert_eq!(results1["test_metric"].total_checks, 2);
        assert_eq!(results1["test_metric"].successes, 1);
    }

    // Second simulation - should start with clean state
    {
        let sim2 = SimWorld::new_with_seed(123);
        sometimes_assert!(test_metric, true, "Test metric");

        let results2 = sim2.assertion_results();
        assert_eq!(results2["test_metric"].total_checks, 1);
        assert_eq!(results2["test_metric"].successes, 1);
        assert_eq!(results2["test_metric"].success_rate(), 100.0);
    }
}

#[test]
fn test_simworld_assertion_methods() {
    let sim = SimWorld::new_with_seed(999);

    // Record some assertions
    always_assert!(basic_check, true, "Basic check");
    sometimes_assert!(probabilistic, true, "Probabilistic check");
    sometimes_assert!(probabilistic, false, "Probabilistic check");

    // Test SimWorld assertion methods
    let results = sim.assertion_results();
    assert_eq!(results.len(), 1);  // Only sometimes_assert! is tracked
    assert!(!results.contains_key("basic_check"));  // always_assert! not tracked
    assert!(results.contains_key("probabilistic"));

    // Test manual reset
    sim.reset_assertion_results();
    let empty_results = sim.assertion_results();
    assert!(empty_results.is_empty());
}

#[test]
fn test_complex_simulation_workflow() {
    let sim = SimWorld::new_with_seed(777);

    // Simulate a distributed consensus algorithm
    let node_count = 5;
    let leader_node = 2;

    // Always assertions for invariants
    always_assert!(
        node_count_valid,
        node_count > 0,
        "Must have at least one node"
    );
    always_assert!(
        leader_valid,
        leader_node < node_count,
        "Leader must be valid node"
    );

    // Sometimes assertions for performance characteristics
    for round in 0..10 {
        let consensus_fast = round < 3; // First 3 rounds are fast
        let message_delivered = round % 4 != 3; // 75% delivery rate

        sometimes_assert!(fast_consensus, consensus_fast, "Consensus should be fast");
        sometimes_assert!(
            reliable_delivery,
            message_delivered,
            "Messages should be delivered"
        );
    }

    let results = sim.assertion_results();

    // Only sometimes_assert! calls are tracked now
    assert_eq!(results.len(), 2);
    assert!(!results.contains_key("node_count_valid"));
    assert!(!results.contains_key("leader_valid"));

    // Verify performance metrics
    assert_eq!(results["fast_consensus"].total_checks, 10);
    assert_eq!(results["fast_consensus"].successes, 3);
    assert_eq!(results["fast_consensus"].success_rate(), 30.0);

    assert_eq!(results["reliable_delivery"].total_checks, 10);
    assert_eq!(results["reliable_delivery"].successes, 8);
    assert_eq!(results["reliable_delivery"].success_rate(), 80.0);
}

#[test]
fn test_parallel_assertion_isolation() {
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();

    // Spawn two threads with different assertion patterns
    for thread_id in 0..2 {
        let barrier_clone = Arc::clone(&barrier);
        let handle = thread::spawn(move || {
            let sim = SimWorld::new_with_seed(thread_id as u64);

            // Each thread uses different assertion patterns
            if thread_id == 0 {
                // Thread 0: More successes
                sometimes_assert!(thread_test, true, "Thread test");
                sometimes_assert!(thread_test, true, "Thread test");
                sometimes_assert!(thread_test, false, "Thread test");
            } else {
                // Thread 1: Fewer successes
                sometimes_assert!(thread_test, false, "Thread test");
                sometimes_assert!(thread_test, true, "Thread test");
            }

            barrier_clone.wait();

            let results = sim.assertion_results();
            let stats = &results["thread_test"];

            if thread_id == 0 {
                assert_eq!(stats.total_checks, 3);
                assert_eq!(stats.successes, 2);
            } else {
                assert_eq!(stats.total_checks, 2);
                assert_eq!(stats.successes, 1);
            }

            stats.clone()
        });
        handles.push(handle);
    }

    // Collect results from both threads
    let results: Vec<AssertionStats> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Verify threads maintained separate assertion state
    assert_eq!(results[0].total_checks, 3);
    assert_eq!(results[0].successes, 2);
    assert_eq!(results[1].total_checks, 2);
    assert_eq!(results[1].successes, 1);
}

#[test]
fn test_assertion_with_complex_conditions() {
    let sim = SimWorld::new_with_seed(456);

    let data = vec![1, 2, 3, 4, 5];
    let sum: i32 = data.iter().sum();
    let avg = sum as f64 / data.len() as f64;

    // Complex boolean conditions
    always_assert!(
        data_valid,
        !data.is_empty() && data.len() <= 10,
        "Data should be non-empty and reasonable size"
    );

    sometimes_assert!(
        performance_target,
        avg >= 2.0 && avg <= 4.0 && sum == 15,
        "Should meet performance targets"
    );

    // Nested computations
    let fibonacci = |n: u32| -> u32 {
        match n {
            0 => 0,
            1 => 1,
            _ => {
                let mut a = 0;
                let mut b = 1;
                for _ in 2..=n {
                    let c = a + b;
                    a = b;
                    b = c;
                }
                b
            }
        }
    };

    always_assert!(
        fibonacci_correct,
        fibonacci(5) == 5 && fibonacci(0) == 0,
        "Fibonacci computation should be correct"
    );

    let results = sim.assertion_results();

    // Only sometimes_assert! is tracked now
    assert_eq!(results.len(), 1);
    assert_eq!(results["performance_target"].success_rate(), 100.0);
    // always_assert! calls are no longer tracked
    assert!(!results.contains_key("data_valid"));
    assert!(!results.contains_key("fibonacci_correct"));
}

#[test]
fn test_assertion_macros_with_variables() {
    let sim = SimWorld::new_with_seed(111);

    // Test with various variable types
    let is_leader = true;
    let node_count = 5_u32;
    let latency_ms = 42.5_f64;
    let message = "hello".to_string();

    always_assert!(leadership, is_leader, "Node should be leader");
    sometimes_assert!(scale, node_count > 3, "Should scale well");
    sometimes_assert!(latency, latency_ms < 50.0, "Latency should be low");
    always_assert!(
        message_valid,
        message.starts_with("h"),
        "Message should start with 'h'"
    );

    let results = sim.assertion_results();

    // Only sometimes_assert! is tracked now (2 calls)
    assert_eq!(results.len(), 2);
    assert_eq!(results["scale"].success_rate(), 100.0);
    assert_eq!(results["latency"].success_rate(), 100.0);
    // always_assert! calls are no longer tracked
    assert!(!results.contains_key("leadership"));
    assert!(!results.contains_key("message_valid"));
}

#[test]
fn test_zero_division_edge_case() {
    let stats = AssertionStats::new();
    assert_eq!(stats.success_rate(), 0.0);

    let mut stats = AssertionStats::new();
    stats.record(false);
    assert_eq!(stats.success_rate(), 0.0);
    assert_eq!(stats.total_checks, 1);
    assert_eq!(stats.successes, 0);
}

#[test]
fn test_assertion_stats_edge_cases() {
    let mut stats = AssertionStats::new();

    // Test many successful assertions
    for _ in 0..1000 {
        stats.record(true);
    }
    assert_eq!(stats.success_rate(), 100.0);

    // Add one failure
    stats.record(false);
    let expected_rate = 1000.0 / 1001.0 * 100.0;
    assert!((stats.success_rate() - expected_rate).abs() < 0.01);
}

#[test]
fn test_simulation_with_deterministic_assertions() {
    // Test that same seed produces same assertion patterns with network config
    let sim1 = SimWorld::new_with_seed(888);
    run_deterministic_assertions(&sim1);
    let results1 = sim1.assertion_results();

    let sim2 = SimWorld::new_with_seed(888);
    run_deterministic_assertions(&sim2);
    let results2 = sim2.assertion_results();

    // Results should be identical
    assert_eq!(results1.len(), results2.len());
    for (key, stats1) in &results1 {
        let stats2 = &results2[key];
        assert_eq!(stats1.total_checks, stats2.total_checks);
        assert_eq!(stats1.successes, stats2.successes);
        assert_eq!(stats1.success_rate(), stats2.success_rate());
    }
}

fn run_deterministic_assertions(sim: &SimWorld) {
    // Use network config to get deterministic latencies
    for _i in 0..5 {
        let delay = sim.with_network_config(|config| config.latency.connect_latency.sample());
        let is_fast = delay.as_millis() < 10; // Some will be fast, some slow

        sometimes_assert!(connection_speed, is_fast, "Connections should be fast");
        always_assert!(
            delay_positive,
            delay.as_nanos() > 0,
            "Delay should be positive"
        );
    }
}
