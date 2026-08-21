//! Thin shim over the optional `moonpool-explorer` backend (feature `exploration`).
//!
//! Keeps the rest of the runner free of `#[cfg]` for the common lifecycle calls:
//! assertion-region init/cleanup, the worker-process check, and the code
//! coverage readers. The exploration-only entry points (the `Explorer`
//! controller, the RNG count hook, per-seed stats and recipes) stay behind
//! `#[cfg(feature = "exploration")]` at their call sites since they have no
//! meaning without the backend.

/// Configuration for exploration. Re-exported from the backend when present; an
/// uninhabited stand-in otherwise so `SimulationBuilder`'s
/// `Option<ExplorationConfig>` field (always `None`) still type-checks.
#[cfg(feature = "exploration")]
pub use moonpool_explorer::ExplorationConfig;

/// Uninhabited stand-in (see [`ExplorationConfig`] above).
#[cfg(not(feature = "exploration"))]
#[derive(Debug, Clone)]
pub enum ExplorationConfig {}

/// Initialise the assertion region: `MAP_SHARED` under the explorer (so
/// forked workers share counts and discovery latches), or a plain heap table
/// without it.
pub fn init_assertion_region() {
    #[cfg(feature = "exploration")]
    if let Err(e) = moonpool_explorer::init_assertions() {
        tracing::error!("Failed to initialize assertion table: {e}");
    }
    #[cfg(not(feature = "exploration"))]
    moonpool_assertions::init();
}

/// Tear down the assertion region.
pub fn cleanup_assertion_region() {
    #[cfg(feature = "exploration")]
    moonpool_explorer::cleanup_assertions();
    #[cfg(not(feature = "exploration"))]
    moonpool_assertions::clear();
}

/// Cumulative real code-coverage edge count, or `None` when unavailable.
///
/// Returns `None` without the `exploration` feature, or when the binary was
/// not sancov-instrumented (i.e. not built via `cargo xtask sim run`). When
/// `exploration_active`, reads the controller-merged history; otherwise reads
/// the live BSS counters of the current process (no fork).
#[must_use]
pub fn code_coverage_edges(exploration_active: bool) -> Option<usize> {
    #[cfg(feature = "exploration")]
    {
        if !moonpool_explorer::sancov_is_available() {
            return None;
        }
        Some(if exploration_active {
            moonpool_explorer::sancov_edges_covered()
        } else {
            moonpool_explorer::sancov_edges_covered_live()
        })
    }
    #[cfg(not(feature = "exploration"))]
    {
        let _ = exploration_active;
        None
    }
}

/// Total number of instrumented code edges, or `None` when unavailable.
#[must_use]
pub fn code_coverage_total() -> Option<usize> {
    #[cfg(feature = "exploration")]
    {
        if moonpool_explorer::sancov_is_available() {
            Some(moonpool_explorer::sancov_edge_count())
        } else {
            None
        }
    }
    #[cfg(not(feature = "exploration"))]
    {
        None
    }
}

/// Whether this process is a forked exploration worker (always false without
/// the backend).
#[must_use]
pub fn explorer_is_child() -> bool {
    #[cfg(feature = "exploration")]
    {
        moonpool_explorer::explorer_is_child()
    }
    #[cfg(not(feature = "exploration"))]
    {
        false
    }
}
