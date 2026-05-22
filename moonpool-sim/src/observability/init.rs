//! Convenience initializer for example/sim binaries.

use tracing::Level;

/// Install a basic stdout `tracing_subscriber::fmt` subscriber at `max_level`.
///
/// Each sim binary's `main` previously inlined the same
/// `tracing_subscriber::fmt().with_max_level(level).try_init()` boilerplate.
/// Calling this helper centralizes that pattern. `try_init` is used so
/// repeat calls (e.g. from re-entrant test harnesses) do not panic; the
/// result is intentionally discarded.
pub fn init_sim_tracing(max_level: Level) {
    let _ = tracing_subscriber::fmt()
        .with_max_level(max_level)
        .try_init();
}
