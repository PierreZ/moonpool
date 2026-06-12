//! Discovery hook: the one-way coupling surface to an exploration backend.
//!
//! The accounting layer ([`crate::slots`], [`crate::buckets`]) calls these hooks
//! at the exact points where something "new" happens — a first Sometimes/Reachable
//! pass, a numeric watermark improvement, a frontier advance, or an each-bucket
//! hit. With no hook installed (the default — wasm, macOS, plain native runs) the
//! calls are no-ops and accounting is pure. An exploration backend
//! (`moonpool-explorer`) installs hooks that mark coverage and dispatch forks.
//!
//! This mirrors `moonpool-explorer`'s own `set_rng_hooks` pattern, inverted: there
//! the sim hands the explorer RNG hooks; here the explorer hands this crate its
//! discovery hooks. The function pointers are stored in a thread-local `Cell` set
//! before forking, so forked children inherit them.

use std::cell::Cell;

/// Discovery callbacks installed by an exploration backend.
///
/// Each field is a plain function pointer (defaults are no-ops). The accounting
/// layer never knows what these do — coverage bitmaps and fork dispatch live
/// entirely behind these pointers.
#[derive(Clone, Copy)]
pub struct DiscoveryHooks {
    /// A slot assertion (bool Sometimes/Reachable first pass, numeric watermark
    /// improvement, or frontier advance) made a discovery. Receives the slot
    /// index and the message hash.
    pub on_slot_discovery: fn(slot_idx: usize, hash: u32),
    /// An each-bucket assertion was hit (called on every invocation). Receives
    /// the bucket hash for coverage marking.
    pub on_bucket_mark: fn(hash: u32),
    /// An each-bucket assertion made a first discovery or quality improvement.
    /// Receives the message label and a slot index.
    pub on_bucket_split: fn(label: &str, slot_idx: usize),
}

fn noop_slot(_: usize, _: u32) {}
fn noop_mark(_: u32) {}
fn noop_split(_: &str, _: usize) {}

impl DiscoveryHooks {
    /// Hooks that do nothing — pure accounting, no exploration.
    pub const NOOP: Self = Self {
        on_slot_discovery: noop_slot,
        on_bucket_mark: noop_mark,
        on_bucket_split: noop_split,
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

/// Install the discovery hooks. Must be called before forking; children inherit
/// the hooks via thread-local storage.
pub fn set_discovery_hooks(hooks: DiscoveryHooks) {
    HOOKS.with(|h| h.set(hooks));
}

/// Remove any installed hooks, reverting to pure accounting.
pub fn clear_discovery_hooks() {
    HOOKS.with(|h| h.set(DiscoveryHooks::NOOP));
}

pub(crate) fn on_slot_discovery(slot_idx: usize, hash: u32) {
    HOOKS.with(|h| (h.get().on_slot_discovery)(slot_idx, hash));
}

pub(crate) fn on_bucket_mark(hash: u32) {
    HOOKS.with(|h| (h.get().on_bucket_mark)(hash));
}

pub(crate) fn on_bucket_split(label: &str, slot_idx: usize) {
    HOOKS.with(|h| (h.get().on_bucket_split)(label, slot_idx));
}
