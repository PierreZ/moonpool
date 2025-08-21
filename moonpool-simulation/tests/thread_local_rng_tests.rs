use moonpool_simulation::{
    LatencyRange, NetworkConfiguration, SimWorld, reset_sim_rng, set_sim_seed, sim_random,
    sim_random_range,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

#[test]
fn test_thread_local_rng_determinism() {
    // Test that same seed produces identical sequences
    set_sim_seed(42);
    let value1: f64 = sim_random();
    let value2: u32 = sim_random();
    let value3: bool = sim_random();

    // Reset to same seed
    set_sim_seed(42);
    assert_eq!(value1, sim_random::<f64>());
    assert_eq!(value2, sim_random::<u32>());
    assert_eq!(value3, sim_random::<bool>());
}

#[test]
fn test_simworld_with_seed_determinism() {
    // Test that SimWorld::new_with_seed produces deterministic behavior
    let sim1 = SimWorld::new_with_seed(123);
    let delay1 = sim1.with_network_config(|config| config.latency.connect_latency.sample());

    let sim2 = SimWorld::new_with_seed(123);
    let delay2 = sim2.with_network_config(|config| config.latency.connect_latency.sample());

    assert_eq!(delay1, delay2);
}

#[test]
fn test_simworld_different_seeds_different_behavior() {
    let sim1 = SimWorld::new_with_seed(1);
    let delay1 = sim1.with_network_config(|config| config.latency.connect_latency.sample());

    let sim2 = SimWorld::new_with_seed(2);
    let delay2 = sim2.with_network_config(|config| config.latency.connect_latency.sample());

    // Different seeds should produce different delays (with very high probability)
    assert_ne!(delay1, delay2);
}

#[test]
fn test_consecutive_simulations_same_thread() {
    // Test that consecutive simulations with different seeds work correctly
    let mut delays = Vec::new();

    for seed in [1, 2, 3, 4, 5] {
        let sim = SimWorld::new_with_seed(seed);
        let delay = sim.with_network_config(|config| config.latency.write_latency.sample());
        delays.push(delay);
    }

    // Repeat with same seeds - should get identical delays
    for (i, seed) in [1, 2, 3, 4, 5].iter().enumerate() {
        let sim = SimWorld::new_with_seed(*seed);
        let delay = sim.with_network_config(|config| config.latency.write_latency.sample());
        assert_eq!(delays[i], delay, "Delay mismatch for seed {}", seed);
    }
}

#[test]
fn test_latency_range_determinism() {
    let range = LatencyRange::new(Duration::from_millis(10), Duration::from_millis(5));

    set_sim_seed(42);
    let delay1 = range.sample();
    let delay2 = range.sample();

    set_sim_seed(42);
    assert_eq!(delay1, range.sample());
    assert_eq!(delay2, range.sample());
}

#[test]
fn test_latency_range_bounds() {
    let range = LatencyRange::new(Duration::from_millis(100), Duration::from_millis(50));

    set_sim_seed(123);
    for _ in 0..100 {
        let delay = range.sample();
        assert!(delay >= Duration::from_millis(100));
        assert!(delay <= Duration::from_millis(150));
    }
}

#[test]
fn test_latency_range_fixed() {
    let range = LatencyRange::fixed(Duration::from_millis(42));

    set_sim_seed(999);
    for _ in 0..10 {
        assert_eq!(range.sample(), Duration::from_millis(42));
    }
}

#[test]
fn test_reset_clears_state() {
    set_sim_seed(42);
    let _advance1: f64 = sim_random();
    let _advance2: f64 = sim_random();
    let after_advance: f64 = sim_random();

    // Reset and set same seed - should get first value, not third
    reset_sim_rng();
    set_sim_seed(42);
    let first_value: f64 = sim_random();

    // Should be different because reset cleared the advanced state
    assert_ne!(after_advance, first_value);
}

#[test]
fn test_parallel_thread_isolation() {
    // Test that different threads maintain separate RNG state
    let thread1_finished = Arc::new(AtomicBool::new(false));
    let thread2_finished = Arc::new(AtomicBool::new(false));

    let thread1_finished_clone = thread1_finished.clone();
    let thread2_finished_clone = thread2_finished.clone();

    let handle1 = thread::spawn(move || {
        set_sim_seed(1);
        let values: Vec<f64> = (0..10).map(|_| sim_random()).collect();

        // Reset and verify same sequence
        set_sim_seed(1);
        for (i, &expected) in values.iter().enumerate() {
            let actual: f64 = sim_random();
            assert_eq!(expected, actual, "Thread 1 mismatch at index {}", i);
        }

        thread1_finished_clone.store(true, Ordering::Relaxed);
        values
    });

    let handle2 = thread::spawn(move || {
        set_sim_seed(2);
        let values: Vec<u32> = (0..10).map(|_| sim_random()).collect();

        // Reset and verify same sequence
        set_sim_seed(2);
        for (i, &expected) in values.iter().enumerate() {
            let actual: u32 = sim_random();
            assert_eq!(expected, actual, "Thread 2 mismatch at index {}", i);
        }

        thread2_finished_clone.store(true, Ordering::Relaxed);
        values
    });

    let values1 = handle1.join().unwrap();
    let values2 = handle2.join().unwrap();

    // Verify both threads completed
    assert!(thread1_finished.load(Ordering::Relaxed));
    assert!(thread2_finished.load(Ordering::Relaxed));

    // Verify threads produced different sequences (different seeds)
    // Convert u32 to f64 for comparison
    let values2_as_f64: Vec<f64> = values2.iter().map(|&x| x as f64).collect();
    assert_ne!(values1, values2_as_f64);
}

#[test]
fn test_simworld_network_config_integration() {
    // Test that network configuration works with thread-local RNG
    let config = NetworkConfiguration::default();
    let sim = SimWorld::new_with_network_config_and_seed(config, 456);

    // Sample various latencies
    let bind_delay = sim.with_network_config(|config| config.latency.bind_latency.sample());
    let connect_delay = sim.with_network_config(|config| config.latency.connect_latency.sample());
    let read_delay = sim.with_network_config(|config| config.latency.read_latency.sample());
    let write_delay = sim.with_network_config(|config| config.latency.write_latency.sample());

    // All should be within expected bounds
    assert!(bind_delay >= Duration::from_micros(50));
    assert!(connect_delay >= Duration::from_millis(1));
    assert!(read_delay >= Duration::from_micros(10));
    assert!(write_delay >= Duration::from_micros(100));

    // Repeat with same seed - should get identical values
    let sim2 = SimWorld::new_with_network_config_and_seed(NetworkConfiguration::default(), 456);
    assert_eq!(
        bind_delay,
        sim2.with_network_config(|config| config.latency.bind_latency.sample())
    );
    assert_eq!(
        connect_delay,
        sim2.with_network_config(|config| config.latency.connect_latency.sample())
    );
    assert_eq!(
        read_delay,
        sim2.with_network_config(|config| config.latency.read_latency.sample())
    );
    assert_eq!(
        write_delay,
        sim2.with_network_config(|config| config.latency.write_latency.sample())
    );
}

#[test]
fn test_sim_random_range_determinism() {
    set_sim_seed(789);
    let range_value1 = sim_random_range(100..1000);
    let range_value2 = sim_random_range(0.0..10.0);

    set_sim_seed(789);
    assert_eq!(range_value1, sim_random_range(100..1000));
    assert_eq!(range_value2, sim_random_range(0.0..10.0));
}

#[test]
fn test_complex_simulation_sequence_determinism() {
    // Test a complex sequence of operations that mirrors real simulation usage
    let results1 = run_complex_simulation_sequence(12345);
    let results2 = run_complex_simulation_sequence(12345);

    assert_eq!(results1, results2);
}

fn run_complex_simulation_sequence(seed: u64) -> Vec<Duration> {
    let sim = SimWorld::new_with_seed(seed);
    let mut results = Vec::new();

    // Simulate various network operations
    for _ in 0..5 {
        results.push(sim.with_network_config(|config| config.latency.bind_latency.sample()));
        results.push(sim.with_network_config(|config| config.latency.connect_latency.sample()));
        results.push(sim.with_network_config(|config| config.latency.write_latency.sample()));
        results.push(sim.with_network_config(|config| config.latency.read_latency.sample()));
    }

    results
}

#[test]
fn test_multiple_simulations_same_thread_no_contamination() {
    // Test that multiple simulations on same thread don't contaminate each other
    let mut all_results = Vec::new();

    // Run multiple simulations with different seeds
    for seed in [100, 200, 300] {
        let sim = SimWorld::new_with_seed(seed);
        let mut sim_results = Vec::new();

        for _ in 0..3 {
            sim_results
                .push(sim.with_network_config(|config| config.latency.connect_latency.sample()));
        }

        all_results.push(sim_results);
    }

    // Repeat the same sequence - should get identical results
    for (i, seed) in [100, 200, 300].iter().enumerate() {
        let sim = SimWorld::new_with_seed(*seed);
        let mut sim_results = Vec::new();

        for _ in 0..3 {
            sim_results
                .push(sim.with_network_config(|config| config.latency.connect_latency.sample()));
        }

        assert_eq!(
            all_results[i], sim_results,
            "Results mismatch for seed {}",
            seed
        );
    }
}
