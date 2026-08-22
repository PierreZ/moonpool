//! Random provider contract: the invariants any drop-in for `rand` must hold.
//!
//! Seed-reproducibility is intentionally excluded — that is a simulation-only
//! property (production uses a thread-local RNG). The contract asserts only the
//! distribution bounds that both implementations share.

use moonpool_core::{Providers, RandomProvider};

/// Assert the [`RandomProvider`] honours its documented ranges and probabilities.
pub(crate) fn random_contract<P: Providers>(p: &P) {
    let random = p.random();

    for _ in 0..100 {
        assert!(
            !random.random_bool(0.0),
            "random_bool(0.0) must always be false"
        );
        assert!(
            random.random_bool(1.0),
            "random_bool(1.0) must always be true"
        );

        assert_eq!(
            random.random_range(5..6),
            5,
            "a single-value range must return its only element"
        );

        let in_range: u32 = random.random_range(10..20);
        assert!(
            (10..20).contains(&in_range),
            "random_range(10..20) out of bounds: {in_range}"
        );

        let ratio = random.random_ratio();
        assert!(
            (0.0..1.0).contains(&ratio),
            "random_ratio() must be in [0.0, 1.0): {ratio}"
        );
    }

    // A typed draw compiles and runs through the bundle.
    let _value: u64 = random.random();
}
