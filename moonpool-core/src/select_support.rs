//! Runtime support for the deterministic [`select!`](crate::select) macro.
//!
//! The expansion emitted by `select!` draws a branch offset from
//! [`select_offset`] on every poll, exactly like tokio's fair mode. Where
//! that offset comes from decides the determinism story:
//!
//! - **Simulation**: moonpool-sim installs an override per iteration via
//!   [`set_select_offset_override`], backed by a seeded RNG stream. Same seed,
//!   same offsets, exact replay; different seeds explore different branch
//!   orderings.
//! - **Production** (no override installed): a thread-local [`SmallRng`]
//!   lazily seeded from [`std::collections::hash_map::RandomState`] (the same
//!   entropy trick tokio's own `FastRand` uses). Behaviorally equivalent to
//!   tokio's fair `select!`, and wasm-clean: seeding from a `u64` pulls no
//!   `getrandom`, no OS calls beyond what `std` already does for hashers.
//!
//! The hook detects the *simulation*, not tokio: the expansion is identical
//! everywhere, only the number source changes.

use std::cell::{Cell, RefCell};

use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};

/// A branch-offset source for [`select!`](crate::select): receives the branch
/// count `n` and returns the index of the branch to poll first (values are
/// reduced modulo `n`).
pub type SelectOffsetFn = fn(u32) -> u32;

thread_local! {
    /// Offset source installed by the simulation runtime (None in production).
    static OFFSET_OVERRIDE: Cell<Option<SelectOffsetFn>> = const { Cell::new(None) };
    /// Lazily seeded entropy fallback (None until first fair-mode draw).
    static FALLBACK_RNG: RefCell<Option<SmallRng>> = const { RefCell::new(None) };
}

/// Install (or clear, with `None`) the branch-offset source used by
/// [`select!`](crate::select) on the current thread.
///
/// moonpool-sim installs a seeded source per simulation iteration; leaving it
/// uninstalled gives entropy-based offsets, which is the correct production
/// behavior.
pub fn set_select_offset_override(source: Option<SelectOffsetFn>) {
    OFFSET_OVERRIDE.with(|cell| cell.set(source));
}

/// Draw the branch start offset for one `select!` execution.
///
/// Not part of the public API: only `select!` expansions call this.
#[doc(hidden)]
pub fn select_offset(branches: u32) -> u32 {
    if branches <= 1 {
        return 0;
    }
    match OFFSET_OVERRIDE.with(Cell::get) {
        Some(source) => source(branches) % branches,
        None => entropy_offset(branches),
    }
}

/// Entropy fallback: a `RandomState`-seeded thread-local [`SmallRng`].
fn entropy_offset(branches: u32) -> u32 {
    FALLBACK_RNG.with(|cell| {
        let mut rng = cell.borrow_mut();
        let rng = rng.get_or_insert_with(|| {
            use std::hash::{BuildHasher, Hasher};
            let seed = std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish();
            SmallRng::seed_from_u64(seed)
        });
        rng.random_range(0..branches)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_one_branch_are_always_offset_zero() {
        assert_eq!(select_offset(0), 0);
        assert_eq!(select_offset(1), 0);
    }

    #[test]
    fn fallback_stays_in_range() {
        set_select_offset_override(None);
        for _ in 0..1000 {
            let offset = select_offset(7);
            assert!(offset < 7);
        }
    }

    #[test]
    fn override_wins_and_is_reduced_modulo_branches() {
        fn always_five(_branches: u32) -> u32 {
            5
        }
        set_select_offset_override(Some(always_five));
        assert_eq!(select_offset(8), 5);
        assert_eq!(select_offset(3), 2); // 5 % 3
        set_select_offset_override(None);
    }
}
