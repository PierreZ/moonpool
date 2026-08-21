//! Discovery hook: the one-way coupling surface to an exploration backend.
//!
//! The accounting layer ([`crate::slots`], [`crate::buckets`]) calls the hook
//! at the exact points where something *globally new* happens — a first
//! Sometimes/Reachable pass, a numeric watermark improvement, a frontier
//! advance, a new each-bucket identity, or an each-bucket quality improvement.
//! Every one of these transitions is guarded by an atomic latch (CAS) in the
//! accounting region, so across any number of processes sharing that region
//! the hook fires **exactly once per distinct discovery**.
//!
//! With no hook installed (the default — wasm, plain native runs) the calls
//! are no-ops and accounting is pure. An exploration backend
//! (`moonpool-explorer`) installs a hook that records the discovery into a
//! per-run journal; the exploration controller later turns journals into
//! follow-up work. The hook itself must never block, fork, or recurse into
//! the accounting layer.
//!
//! The function pointer is stored in a thread-local `Cell` set before any
//! worker processes are forked, so forked children inherit it.

use std::cell::Cell;

/// What kind of discovery the accounting layer observed.
///
/// The exploration controller can use this to rank discoveries: watermark and
/// frontier improvements represent monotonic *progress*, while first passes
/// and new buckets represent *coverage*.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryKind {
    /// A `Sometimes`/`Reachable` slot passed for the first time.
    SometimesPass = 0,
    /// A `NumericSometimes` watermark improved past its previous best.
    WatermarkImprovement = 1,
    /// A `BooleanSometimesAll` frontier advanced (more bools simultaneously true).
    FrontierAdvance = 2,
    /// An `assert_sometimes_each!` bucket was hit for the first time.
    BucketFirst = 3,
    /// An `assert_sometimes_each!` bucket's quality score improved.
    BucketQuality = 4,
}

impl DiscoveryKind {
    /// Convert from raw u8, returning `None` for invalid values.
    #[must_use]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::SometimesPass),
            1 => Some(Self::WatermarkImprovement),
            2 => Some(Self::FrontierAdvance),
            3 => Some(Self::BucketFirst),
            4 => Some(Self::BucketQuality),
            _ => None,
        }
    }
}

/// Discovery callback installed by an exploration backend.
///
/// A plain function pointer (default is a no-op). The accounting layer never
/// knows what it does — journaling and exploration policy live entirely
/// behind this pointer.
#[derive(Clone, Copy)]
pub struct DiscoveryHooks {
    /// A globally new discovery was made (see [`DiscoveryKind`]). Fires at
    /// most once per distinct discovery across all processes sharing the
    /// accounting region.
    pub on_discovery: fn(kind: DiscoveryKind),
}

fn noop_discovery(_: DiscoveryKind) {}

impl DiscoveryHooks {
    /// Hooks that do nothing — pure accounting, no exploration.
    pub const NOOP: Self = Self {
        on_discovery: noop_discovery,
    };
}

impl Default for DiscoveryHooks {
    fn default() -> Self {
        Self::NOOP
    }
}

thread_local! {
    static HOOKS: Cell<DiscoveryHooks> = const { Cell::new(DiscoveryHooks::NOOP) };
}

/// Install the discovery hook. Must be called before forking; children inherit
/// the hook via thread-local storage.
pub fn set_discovery_hooks(hooks: DiscoveryHooks) {
    HOOKS.with(|h| h.set(hooks));
}

/// Remove any installed hook, reverting to pure accounting.
pub fn clear_discovery_hooks() {
    HOOKS.with(|h| h.set(DiscoveryHooks::NOOP));
}

pub(crate) fn on_discovery(kind: DiscoveryKind) {
    HOOKS.with(|h| (h.get().on_discovery)(kind));
}
