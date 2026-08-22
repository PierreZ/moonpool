//! Wall-clock helpers that degrade gracefully on `wasm32-unknown-unknown`.
//!
//! On that target `std::time::Instant::now()` and `SystemTime::now()` compile
//! but **panic at runtime** ("time not implemented on this platform"). The sim
//! drives logical time through its own event queue, so these are only used for
//! harness bookkeeping (report elapsed, `TimeLimit`, the default base seed).
//! Native builds use `std::time` directly with zero overhead; wasm builds get a
//! zero-duration stub so a single-seed run executes instead of aborting.

#[cfg(target_family = "wasm")]
use std::time::Duration;

/// Monotonic instant for harness timing. Native: re-exported `std::time::Instant`.
#[cfg(not(target_family = "wasm"))]
pub(crate) use std::time::Instant;

/// wasm stand-in: never advances (`elapsed()` is always `Duration::ZERO`), which
/// makes `TimeLimit` behave as a single pass and report timings show `0ms`.
#[cfg(target_family = "wasm")]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Instant(Duration);

#[cfg(target_family = "wasm")]
impl Instant {
    pub(crate) fn now() -> Self {
        Self(Duration::ZERO)
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.0
    }
}

/// Entropy for the default base seed when the caller did not set one explicitly.
///
/// Native: wall-clock nanoseconds (falling back to a fixed value if the clock is
/// before the epoch). wasm: a fixed seed plus a warning — deterministic runs on
/// wasm should always set a seed explicitly.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn default_base_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(12345, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

#[cfg(target_family = "wasm")]
pub(crate) fn default_base_seed() -> u64 {
    tracing::warn!(
        "no base seed set on wasm; using fixed seed 0 (set a seed explicitly for determinism)"
    );
    0
}
