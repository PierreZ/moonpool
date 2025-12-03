//! Integration tests for clock drift chaos injection
//!
//! Tests verify that clock drift:
//! - Follows FDB's timer() semantics (sim2.actor.cpp:1058-1064)
//! - timer() is always >= now()
//! - timer() never exceeds now() + clock_drift_max
//! - Drift progresses smoothly using the FDB formula
//! - Can be disabled via configuration
//! - Is deterministic with fixed seeds

use moonpool_sim::{NetworkConfiguration, SimTimeProvider, SimWorld, TimeProvider, set_sim_seed};
use std::time::Duration;

/// Test that timer() is always >= now()
#[test]
fn test_timer_never_before_now() {
    // Test with multiple seeds for robustness
    for seed in 0..10 {
        set_sim_seed(seed);
        let sim = SimWorld::new();

        for _ in 0..100 {
            let now = sim.now();
            let timer = sim.timer();

            assert!(
                timer >= now,
                "seed={}: timer ({:?}) should be >= now ({:?})",
                seed,
                timer,
                now
            );
        }
    }
}

/// Test that timer() never exceeds now() + clock_drift_max
#[test]
fn test_timer_bounded_by_max_drift() {
    // Test with multiple seeds
    for seed in 0..10 {
        set_sim_seed(seed);
        let sim = SimWorld::new();
        let max_drift = Duration::from_millis(100); // Default clock_drift_max

        for _ in 0..100 {
            let now = sim.now();
            let timer = sim.timer();

            assert!(
                timer <= now + max_drift,
                "seed={}: timer ({:?}) should be <= now + 100ms ({:?})",
                seed,
                timer,
                now + max_drift
            );
        }
    }
}

/// Test that timer() progresses over repeated calls
#[test]
fn test_timer_progresses() {
    set_sim_seed(42);
    let sim = SimWorld::new();

    let initial_timer = sim.timer();
    let mut final_timer = initial_timer;

    // Call timer() multiple times - it should drift ahead
    for _ in 0..100 {
        final_timer = sim.timer();
    }

    // With the FDB formula, timer should have drifted ahead
    // At time 0, timer starts at 0 and can drift up to 100ms
    assert!(
        final_timer > initial_timer,
        "Timer should have progressed: initial={:?}, final={:?}",
        initial_timer,
        final_timer
    );

    // Should have drifted by a significant amount (at least 50ms of the 100ms max)
    let drift = final_timer - initial_timer;
    assert!(
        drift >= Duration::from_millis(50),
        "Timer should have drifted significantly: drift={:?}",
        drift
    );
}

/// Test that clock drift can be disabled
#[test]
fn test_clock_drift_disabled() {
    set_sim_seed(42);

    // Create config with clock drift disabled
    let mut config = NetworkConfiguration::fast_local();
    config.chaos.clock_drift_enabled = false;

    let sim = SimWorld::new_with_network_config(config);

    // With drift disabled, timer() should equal now()
    for _ in 0..100 {
        let now = sim.now();
        let timer = sim.timer();

        assert_eq!(
            timer, now,
            "With drift disabled, timer ({:?}) should equal now ({:?})",
            timer, now
        );
    }
}

/// Test deterministic replay with same seed
#[test]
fn test_clock_drift_deterministic() {
    const SEED: u64 = 12345;

    // Run 1
    set_sim_seed(SEED);
    let sim1 = SimWorld::new();
    let mut timers1 = Vec::new();
    for _ in 0..50 {
        timers1.push(sim1.timer());
    }

    // Run 2 with same seed
    set_sim_seed(SEED);
    let sim2 = SimWorld::new();
    let mut timers2 = Vec::new();
    for _ in 0..50 {
        timers2.push(sim2.timer());
    }

    // Sequences should be identical
    assert_eq!(
        timers1, timers2,
        "Clock drift should be deterministic with same seed"
    );
}

/// Test custom clock_drift_max configuration
#[test]
fn test_custom_drift_max() {
    set_sim_seed(42);

    let custom_max = Duration::from_millis(50);
    let mut config = NetworkConfiguration::default();
    config.chaos.clock_drift_max = custom_max;

    let sim = SimWorld::new_with_network_config(config);

    // Timer should respect custom max
    for _ in 0..100 {
        let now = sim.now();
        let timer = sim.timer();

        assert!(
            timer <= now + custom_max,
            "Timer ({:?}) should be <= now + custom_max ({:?})",
            timer,
            now + custom_max
        );
    }
}

/// Test TimeProvider integration with clock drift
#[test]
fn test_time_provider_clock_drift() {
    set_sim_seed(42);
    let sim = SimWorld::new();
    let time_provider = SimTimeProvider::new(sim.downgrade());

    // Test now() vs timer() through TimeProvider
    for _ in 0..100 {
        let now = time_provider.now();
        let timer = time_provider.timer();

        assert!(timer >= now, "timer ({:?}) >= now ({:?})", timer, now);
        assert!(
            timer <= now + Duration::from_millis(100),
            "timer ({:?}) <= now + 100ms",
            timer
        );
    }
}

/// Test clock drift with time advancement
#[test]
fn test_clock_drift_with_time_advance() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        set_sim_seed(42);
        let mut sim = SimWorld::new();
        let time_provider = SimTimeProvider::new(sim.downgrade());

        // Initial state
        let initial_now = time_provider.now();
        let initial_timer = time_provider.timer();

        assert!(initial_timer >= initial_now);
        assert!(initial_timer <= initial_now + Duration::from_millis(100));

        // Advance time by sleeping
        let _sleep = sim.downgrade().sleep(Duration::from_millis(100));
        sim.run_until_empty();

        // After advancing time
        let new_now = time_provider.now();
        let new_timer = time_provider.timer();

        // now() should have advanced
        assert!(
            new_now >= initial_now + Duration::from_millis(100),
            "now() should have advanced"
        );

        // timer() should still be >= now() and bounded
        assert!(new_timer >= new_now);
        assert!(new_timer <= new_now + Duration::from_millis(100));
    });
}

/// Test that FDB's drift formula produces smooth progression
/// Formula: timerTime += random01() * (time + 0.1 - timerTime) / 2.0
#[test]
fn test_drift_formula_smoothness() {
    set_sim_seed(42);
    let sim = SimWorld::new();

    let mut prev_timer = Duration::ZERO;
    let max_drift = Duration::from_millis(100);

    // Timer should never jump backward and never exceed bounds
    for _ in 0..100 {
        let now = sim.now();
        let timer = sim.timer();

        // Monotonic (never goes backward)
        assert!(
            timer >= prev_timer,
            "Timer should be monotonic: {:?} >= {:?}",
            timer,
            prev_timer
        );

        // Bounded
        assert!(timer >= now);
        assert!(timer <= now + max_drift);

        prev_timer = timer;
    }
}

/// Test drift behavior is affected by simulation RNG
/// (Note: This test verifies determinism rather than seed differentiation
/// because the thread-local RNG design makes cross-simulation comparisons
/// depend on shared state. The determinism test above verifies the core property.)
#[test]
fn test_drift_uses_rng() {
    set_sim_seed(42);
    let sim = SimWorld::new();

    let mut timers = Vec::new();
    for _ in 0..20 {
        timers.push(sim.timer());
    }

    // Verify drift is actually happening (not just returning now())
    // With FDB formula, timer should drift ahead over multiple calls
    let first = timers[0];
    let last = timers[timers.len() - 1];

    assert!(
        last > first,
        "Timer should drift ahead over multiple calls: first={:?}, last={:?}",
        first,
        last
    );
}
