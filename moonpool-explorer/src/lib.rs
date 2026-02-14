//! Fork-based multiverse exploration for deterministic simulation testing.
//!
//! This crate extends moonpool-sim's single-seed simulation with fork-based
//! exploration. When a `sometimes_assert!` fires successfully for the first
//! time, the explorer forks child processes with different RNG seeds to
//! explore alternate timelines from that discovery point.
//!
//! # Architecture
//!
//! ```text
//! moonpool-explorer (this crate)  -- leaf, only depends on libc
//!      ^
//! moonpool-sim                    -- wires RNG hooks at init
//! ```
//!
//! Communication with the RNG uses function pointers set via [`set_rng_hooks`].
//! This crate has no knowledge of moonpool internals.
//!
//! # Usage
//!
//! ```ignore
//! // In moonpool-sim's runner:
//! moonpool_explorer::set_rng_hooks(get_rng_call_count, |seed| {
//!     set_sim_seed(seed);
//!     reset_rng_call_count();
//! });
//!
//! moonpool_explorer::init(ExplorationConfig {
//!     max_depth: 2,
//!     children_per_fork: 4,
//!     global_energy: 100,
//!     adaptive: None,
//! })?;
//!
//! // ... run simulation ...
//!
//! moonpool_explorer::cleanup();
//! ```

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

pub mod assertion_slots;
pub mod context;
pub mod coverage;
pub mod each_buckets;
pub mod energy;
pub mod fork_loop;
pub mod replay;
pub mod shared_mem;
pub mod shared_stats;

// Re-exports for the public API
pub use assertion_slots::maybe_fork_on_assertion;
pub use context::{explorer_is_child, set_rng_hooks};
pub use each_buckets::{EachBucket, assertion_sometimes_each, each_bucket_read_all};
pub use fork_loop::{AdaptiveConfig, exit_child};
pub use replay::{ParseTimelineError, format_timeline, parse_timeline};
pub use shared_stats::{ExplorationStats, get_bug_recipe, get_exploration_stats};

use context::{
    ASSERTION_TABLE, COVERAGE_BITMAP_PTR, EACH_BUCKET_PTR, ENERGY_BUDGET_PTR, SHARED_RECIPE,
    SHARED_STATS, VIRGIN_MAP_PTR,
};

/// Configuration for exploration.
#[derive(Debug, Clone)]
pub struct ExplorationConfig {
    /// Maximum fork depth (0 = no forking).
    pub max_depth: u32,
    /// Number of children to fork at each discovery point (fixed-count mode).
    pub children_per_fork: u32,
    /// Global energy budget (total number of fork operations allowed).
    pub global_energy: i64,
    /// Optional adaptive forking configuration.
    /// When `None`, uses fixed `children_per_fork` (backward compatible).
    /// When `Some`, uses coverage-yield-driven batch forking with 3-level energy.
    pub adaptive: Option<fork_loop::AdaptiveConfig>,
}

/// Total size of the assertion slot table in bytes.
const ASSERTION_TABLE_SIZE: usize =
    assertion_slots::MAX_ASSERTION_SLOTS * std::mem::size_of::<assertion_slots::AssertionSlot>();

/// Initialize the exploration framework.
///
/// Allocates shared memory for cross-process state and activates exploration.
/// Must be called after [`set_rng_hooks`] and before the simulation loop.
///
/// # Errors
///
/// Returns an error if shared memory allocation fails.
pub fn init(config: ExplorationConfig) -> Result<(), std::io::Error> {
    // Allocate shared memory regions
    let stats_ptr = shared_stats::init_shared_stats(config.global_energy)?;
    let recipe_ptr = shared_stats::init_shared_recipe()?;
    let virgin_ptr = shared_mem::alloc_shared(coverage::COVERAGE_MAP_SIZE)?;
    let bitmap_ptr = shared_mem::alloc_shared(coverage::COVERAGE_MAP_SIZE)?;
    let table_ptr = shared_mem::alloc_shared(ASSERTION_TABLE_SIZE)?;
    let each_bucket_ptr = shared_mem::alloc_shared(each_buckets::EACH_BUCKET_MEM_SIZE)?;

    // Allocate energy budget if adaptive mode is configured
    let energy_ptr = if let Some(ref adaptive) = config.adaptive {
        energy::init_energy_budget(config.global_energy, adaptive.per_mark_energy)?
    } else {
        std::ptr::null_mut()
    };

    // Store pointers in thread-local context
    SHARED_STATS.with(|c| c.set(stats_ptr));
    SHARED_RECIPE.with(|c| c.set(recipe_ptr));
    VIRGIN_MAP_PTR.with(|c| c.set(virgin_ptr));
    COVERAGE_BITMAP_PTR.with(|c| c.set(bitmap_ptr));
    ASSERTION_TABLE.with(|c| c.set(table_ptr as *mut assertion_slots::AssertionSlot));
    EACH_BUCKET_PTR.with(|c| c.set(each_bucket_ptr));
    ENERGY_BUDGET_PTR.with(|c| c.set(energy_ptr));

    // Activate exploration context
    context::with_ctx_mut(|ctx| {
        ctx.active = true;
        ctx.is_child = false;
        ctx.depth = 0;
        ctx.max_depth = config.max_depth;
        ctx.current_seed = 0;
        ctx.recipe.clear();
        ctx.children_per_fork = config.children_per_fork;
        ctx.adaptive = config.adaptive.clone();
    });

    Ok(())
}

/// Clean up the exploration framework.
///
/// Frees all shared memory and deactivates exploration.
/// Call after the simulation loop completes.
pub fn cleanup() {
    // Deactivate
    context::with_ctx_mut(|ctx| {
        ctx.active = false;
    });

    // Free shared memory regions
    // Safety: these pointers were allocated by init() via alloc_shared()
    unsafe {
        let stats_ptr = SHARED_STATS.with(|c| c.get());
        if !stats_ptr.is_null() {
            shared_mem::free_shared(
                stats_ptr as *mut u8,
                std::mem::size_of::<shared_stats::SharedStats>(),
            );
            SHARED_STATS.with(|c| c.set(std::ptr::null_mut()));
        }

        let recipe_ptr = SHARED_RECIPE.with(|c| c.get());
        if !recipe_ptr.is_null() {
            shared_mem::free_shared(
                recipe_ptr as *mut u8,
                std::mem::size_of::<shared_stats::SharedRecipe>(),
            );
            SHARED_RECIPE.with(|c| c.set(std::ptr::null_mut()));
        }

        let virgin_ptr = VIRGIN_MAP_PTR.with(|c| c.get());
        if !virgin_ptr.is_null() {
            shared_mem::free_shared(virgin_ptr, coverage::COVERAGE_MAP_SIZE);
            VIRGIN_MAP_PTR.with(|c| c.set(std::ptr::null_mut()));
        }

        let bitmap_ptr = COVERAGE_BITMAP_PTR.with(|c| c.get());
        if !bitmap_ptr.is_null() {
            shared_mem::free_shared(bitmap_ptr, coverage::COVERAGE_MAP_SIZE);
            COVERAGE_BITMAP_PTR.with(|c| c.set(std::ptr::null_mut()));
        }

        let table_ptr = ASSERTION_TABLE.with(|c| c.get());
        if !table_ptr.is_null() {
            shared_mem::free_shared(table_ptr as *mut u8, ASSERTION_TABLE_SIZE);
            ASSERTION_TABLE.with(|c| c.set(std::ptr::null_mut()));
        }

        let energy_ptr = ENERGY_BUDGET_PTR.with(|c| c.get());
        if !energy_ptr.is_null() {
            shared_mem::free_shared(
                energy_ptr as *mut u8,
                std::mem::size_of::<energy::EnergyBudget>(),
            );
            ENERGY_BUDGET_PTR.with(|c| c.set(std::ptr::null_mut()));
        }

        let each_bucket_ptr = EACH_BUCKET_PTR.with(|c| c.get());
        if !each_bucket_ptr.is_null() {
            shared_mem::free_shared(each_bucket_ptr, each_buckets::EACH_BUCKET_MEM_SIZE);
            EACH_BUCKET_PTR.with(|c| c.set(std::ptr::null_mut()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_cleanup_cycle() {
        let config = ExplorationConfig {
            max_depth: 2,
            children_per_fork: 4,
            global_energy: 100,
            adaptive: None,
        };

        init(config).expect("init failed");
        assert!(context::explorer_is_active());
        assert!(!context::explorer_is_child());

        // Stats should be available
        let stats = get_exploration_stats().expect("stats should be available");
        assert_eq!(stats.global_energy, 100);
        assert_eq!(stats.total_timelines, 0);
        assert_eq!(stats.fork_points, 0);
        assert_eq!(stats.bug_found, 0);

        cleanup();
        assert!(!context::explorer_is_active());
        assert!(get_exploration_stats().is_none());
    }

    #[test]
    fn test_inactive_by_default() {
        assert!(!context::explorer_is_active());
        assert!(!context::explorer_is_child());
    }

    #[test]
    fn test_maybe_fork_noop_when_inactive() {
        // Should not panic when exploration is not active
        maybe_fork_on_assertion("test_assertion");
    }

    #[test]
    fn test_backward_compat_no_adaptive() {
        let config = ExplorationConfig {
            max_depth: 2,
            children_per_fork: 4,
            global_energy: 100,
            adaptive: None,
        };
        init(config).expect("init failed");

        // Energy budget pointer should be null (old path)
        let has_energy = context::ENERGY_BUDGET_PTR.with(|c| !c.get().is_null());
        assert!(!has_energy);

        cleanup();
    }
}
