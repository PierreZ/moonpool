//! Integration tests for the buggify system

use moonpool_sim::{buggify_init, buggify_reset, reset_sim_rng, set_sim_seed};

#[test]
fn test_buggify_integration() {
    println!("Testing buggify basic functionality...");

    // Test with high probability to ensure we see buggify trigger
    reset_sim_rng();
    set_sim_seed(12345);
    buggify_init(1.0, 1.0); // 100% activation, 100% firing

    let mut fired_count = 0;
    let mut total_tests = 0;

    println!("Testing buggify! macro:");
    for i in 0..20 {
        total_tests += 1;
        if moonpool_sim::buggify!() {
            println!("🐛 buggify!() FIRED at iteration {i}");
            fired_count += 1;
        } else {
            println!("   buggify!() did not fire at iteration {i}");
        }
    }

    println!("Fired {fired_count} out of {total_tests} tests");

    // With 100% probabilities, we should see some firings
    assert!(
        fired_count > 0,
        "Expected at least some buggify firings with 100% probability, got {fired_count}"
    );

    println!("\nTesting buggify_with_prob!(1.0) - should fire at least once:");
    let mut prob_fired_count = 0;
    for i in 0..20 {
        if moonpool_sim::buggify_with_prob!(1.0) {
            println!("🐛 buggify_with_prob!(1.0) FIRED at iteration {i}");
            prob_fired_count += 1;
        } else {
            println!("   buggify_with_prob!(1.0) did not fire at iteration {i}");
        }
    }

    println!("Prob fired {prob_fired_count} out of 20 tests");
    assert!(
        prob_fired_count > 0,
        "Expected at least some buggify_with_prob!(1.0) firings, got {prob_fired_count}"
    );

    buggify_reset();
    println!("\nAfter reset, buggify should never fire:");
    for i in 0..5 {
        if moonpool_sim::buggify!() {
            panic!("❌ ERROR: buggify!() fired after reset at iteration {i}!");
        } else {
            println!("✅ buggify!() correctly disabled after reset (iteration {i})");
        }
    }

    println!("✅ All buggify integration tests passed!");
}

#[test]
fn test_buggify_determinism() {
    const TEST_SEED: u64 = 98765;

    println!("Testing buggify determinism...");

    // Run the same test twice with the same seed
    let mut results1 = Vec::new();
    let mut results2 = Vec::new();

    for run in 0..2 {
        reset_sim_rng();
        set_sim_seed(TEST_SEED);
        buggify_init(0.8, 0.8); // High probability to get some hits

        let results = if run == 0 {
            &mut results1
        } else {
            &mut results2
        };

        // Test multiple different locations
        for i in 0..10 {
            let result = moonpool_sim::buggify_with_prob!(0.7);
            results.push(result);
            println!("Run {}, iteration {}: {}", run + 1, i, result);
        }

        buggify_reset();
    }

    println!("Results 1: {results1:?}");
    println!("Results 2: {results2:?}");

    // Results should be identical for same seed
    assert_eq!(
        results1, results2,
        "Buggify should be deterministic with same seed"
    );

    println!("✅ Buggify determinism test passed!");
}
