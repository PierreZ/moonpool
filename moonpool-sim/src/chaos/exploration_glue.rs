//! Thin shim over the optional `moonpool-explorer` backend (feature `exploration`).
//!
//! Keeps the rest of the runner free of `#[cfg]` for the common lifecycle calls:
//! assertion-region init/cleanup, the forked-child checks, and the explored
//! coverage count. The exploration-only entry points (full `init`, RNG hooks,
//! per-seed energy reset, stats, recipes) stay behind `#[cfg(feature =
//! "exploration")]` at their call sites since they have no meaning without the
//! backend.

/// Configuration for exploration. Re-exported from the backend when present; an
/// uninhabited stand-in otherwise so `SimulationBuilder`'s
/// `Option<ExplorationConfig>` field (always `None`) still type-checks.
#[cfg(feature = "exploration")]
pub use moonpool_explorer::ExplorationConfig;

/// Uninhabited stand-in (see [`ExplorationConfig`] above).
#[cfg(not(feature = "exploration"))]
#[derive(Debug, Clone)]
pub enum ExplorationConfig {}

/// Initialise the assertion region: `MAP_SHARED` + discovery hooks under the
/// explorer, or a plain heap table without it.
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

/// Number of bits set in the explored coverage map (0 without exploration).
#[must_use]
pub fn explored_coverage_bits() -> u32 {
    #[cfg(feature = "exploration")]
    {
        moonpool_explorer::explored_map_bits_set().unwrap_or(0)
    }
    #[cfg(not(feature = "exploration"))]
    {
        0
    }
}

/// Whether this process is a forked exploration child (always false without it).
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

/// Exit a forked exploration child. Only reachable when [`explorer_is_child`]
/// returned true, which never happens without the backend.
pub fn exit_child(code: i32) -> ! {
    #[cfg(feature = "exploration")]
    {
        moonpool_explorer::exit_child(code)
    }
    #[cfg(not(feature = "exploration"))]
    {
        let _ = code;
        unreachable!("exit_child called without the `exploration` feature")
    }
}
