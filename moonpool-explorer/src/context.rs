//! Thread-local exploration context and RNG hooks.
//!
//! Stores per-process exploration state and the function pointers used to
//! communicate with moonpool-sim's RNG system. The RNG hooks are the entire
//! coupling surface between this crate and moonpool-sim.

use std::cell::{Cell, RefCell};

use crate::assertion_slots::AssertionSlot;
use crate::shared_stats::{SharedRecipe, SharedStats};

thread_local! {
    /// Per-process exploration state.
    static EXPLORER_CTX: RefCell<ExplorerCtx> = RefCell::new(ExplorerCtx::inactive());

    /// Function pointer to get current RNG call count from moonpool-sim.
    static RNG_GET_COUNT: Cell<fn() -> u64> = const { Cell::new(|| 0) };

    /// Function pointer to reseed the RNG in moonpool-sim.
    static RNG_RESEED: Cell<fn(u64)> = const { Cell::new(|_| {}) };

    // Shared-memory pointers (set during init, used by fork_loop and assertion_slots)

    /// Pointer to cross-process statistics.
    pub(crate) static SHARED_STATS: Cell<*mut SharedStats> = const { Cell::new(std::ptr::null_mut()) };

    /// Pointer to shared recipe storage for bug-finding timelines.
    pub(crate) static SHARED_RECIPE: Cell<*mut SharedRecipe> = const { Cell::new(std::ptr::null_mut()) };

    /// Pointer to cross-process virgin coverage map.
    pub(crate) static VIRGIN_MAP_PTR: Cell<*mut u8> = const { Cell::new(std::ptr::null_mut()) };

    /// Pointer to per-child coverage bitmap.
    pub(crate) static COVERAGE_BITMAP_PTR: Cell<*mut u8> = const { Cell::new(std::ptr::null_mut()) };

    /// Pointer to shared assertion slot table.
    pub(crate) static ASSERTION_TABLE: Cell<*mut AssertionSlot> = const { Cell::new(std::ptr::null_mut()) };
}

/// Exploration state for the current process.
pub struct ExplorerCtx {
    /// Whether exploration is active.
    pub active: bool,
    /// Whether this process is a forked child.
    pub is_child: bool,
    /// Current fork depth (0 = root).
    pub depth: u32,
    /// Maximum allowed fork depth.
    pub max_depth: u32,
    /// Current seed for this timeline.
    pub current_seed: u64,
    /// Recipe: sequence of `(rng_call_count, child_seed)` pairs describing
    /// the fork points that led to this timeline.
    pub recipe: Vec<(u64, u64)>,
    /// Number of children to fork at each discovery point.
    pub children_per_fork: u32,
}

impl ExplorerCtx {
    /// Create an inactive context (exploration disabled).
    pub fn inactive() -> Self {
        Self {
            active: false,
            is_child: false,
            depth: 0,
            max_depth: 0,
            current_seed: 0,
            recipe: Vec::new(),
            children_per_fork: 0,
        }
    }
}

/// Set the RNG hooks used to communicate with moonpool-sim.
///
/// `get_count` returns the current RNG call count.
/// `reseed` reseeds the RNG with a new seed and resets the call count.
///
/// Must be called before [`crate::init`].
pub fn set_rng_hooks(get_count: fn() -> u64, reseed: fn(u64)) {
    RNG_GET_COUNT.with(|c| c.set(get_count));
    RNG_RESEED.with(|c| c.set(reseed));
}

/// Get the current RNG call count via the registered hook.
pub(crate) fn rng_get_count() -> u64 {
    RNG_GET_COUNT.with(|c| (c.get())())
}

/// Reseed the RNG via the registered hook.
pub(crate) fn rng_reseed(seed: u64) {
    RNG_RESEED.with(|c| (c.get())(seed));
}

/// Read the exploration context.
pub(crate) fn with_ctx<R>(f: impl FnOnce(&ExplorerCtx) -> R) -> R {
    EXPLORER_CTX.with(|ctx| f(&ctx.borrow()))
}

/// Mutate the exploration context.
pub(crate) fn with_ctx_mut<R>(f: impl FnOnce(&mut ExplorerCtx) -> R) -> R {
    EXPLORER_CTX.with(|ctx| f(&mut ctx.borrow_mut()))
}

/// Check if exploration is active.
pub fn explorer_is_active() -> bool {
    with_ctx(|ctx| ctx.active)
}

/// Check if this process is a forked child.
pub fn explorer_is_child() -> bool {
    with_ctx(|ctx| ctx.is_child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_hooks() {
        // Default get_count returns 0
        assert_eq!(rng_get_count(), 0);
        // Default reseed is a no-op (does not panic)
        rng_reseed(42);
    }

    #[test]
    fn test_set_hooks() {
        thread_local! {
            static CALL_COUNT: Cell<u64> = const { Cell::new(0) };
            static LAST_SEED: Cell<u64> = const { Cell::new(0) };
        }

        set_rng_hooks(
            || CALL_COUNT.with(|c| c.get()),
            |seed| LAST_SEED.with(|s| s.set(seed)),
        );

        CALL_COUNT.with(|c| c.set(42));
        assert_eq!(rng_get_count(), 42);

        rng_reseed(123);
        assert_eq!(LAST_SEED.with(|s| s.get()), 123);

        // Reset to defaults
        set_rng_hooks(|| 0, |_| {});
    }

    #[test]
    fn test_inactive_by_default() {
        assert!(!explorer_is_active());
        assert!(!explorer_is_child());
    }
}
