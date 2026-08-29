//! Wall-clock instants that degrade gracefully on `wasm32-unknown-unknown`.
//!
//! On that target `std::time::Instant::now()` compiles but **panics at
//! runtime** ("time not implemented on this platform"). The only wall-clock
//! reads in h2 are the locally-reset-stream expiration bookkeeping in this
//! module's siblings (`stream.rs` sets `reset_at`, `recv.rs` prunes entries
//! older than `reset_stream_duration`). A frozen instant keeps that queue
//! from ever expiring — behaviorally identical to any process whose run is
//! shorter than the (default 30s) expiration window — instead of aborting
//! the whole wasm module on the first locally reset stream.
//!
//! Native builds re-export `std::time::Instant` with zero overhead.

#[cfg(not(target_family = "wasm"))]
pub(crate) use std::time::Instant;

/// wasm stand-in: a frozen clock. `now()` always returns the same instant and
/// `saturating_duration_since` is always zero, so reset-stream entries never
/// look expired.
#[cfg(target_family = "wasm")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Instant;

#[cfg(target_family = "wasm")]
impl Instant {
    pub(crate) fn now() -> Self {
        Instant
    }

    pub(crate) fn saturating_duration_since(&self, _earlier: Instant) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}
