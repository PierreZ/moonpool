//! Per-run discovery journal.
//!
//! During one timeline run, every globally-new discovery signalled by the
//! assertion accounting (see `moonpool-assertions::hooks`) is recorded here as
//! a [`DiscoveryEvent`]: *what* was discovered (kind + semantic state id) and
//! *when* (the simulation RNG call count at that moment). The accounting layer
//! guarantees each distinct discovery is signalled exactly once across all
//! processes sharing the region, so a journal only ever contains discoveries
//! that no earlier timeline had made.
//!
//! The journal lives in a thread-local vector. The controller reads it
//! directly after an in-process run; a forked worker serializes it into its
//! `MAP_SHARED` result slot (see [`crate::worker`]) before exiting.
//!
//! The RNG call count comes from a hook installed by the simulation runner
//! ([`set_rng_count_hook`]) — this crate has no knowledge of how the
//! simulation counts its RNG draws, only that the count is the replay
//! coordinate a [`Recipe`](crate::replay) breakpoint can anchor to.

use std::cell::{Cell, RefCell};

use moonpool_assertions::DiscoveryKind;

/// Maximum number of discovery events retained per run.
///
/// A run that makes more discoveries than this keeps the first
/// `MAX_JOURNAL_ENTRIES`. One run is expanded at most once regardless of how
/// many discoveries it made, so truncation only loses exemplar anchors, never
/// exploration momentum.
pub const MAX_JOURNAL_ENTRIES: usize = 256;

/// One globally-new discovery observed during a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryEvent {
    /// Simulation RNG call count (within the current replay segment) at the
    /// moment of discovery. Appending `(call_count, new_seed)` to the run's
    /// recipe replays up to this exact state and then diverges.
    pub call_count: u64,
    /// What kind of discovery this was.
    pub kind: DiscoveryKind,
    /// Semantic state id: assertion message hash or each-bucket hash.
    pub state_id: u64,
}

impl DiscoveryEvent {
    /// Whether this discovery represents monotonic *progress* (a watermark,
    /// frontier, or quality improvement) rather than plain coverage.
    #[must_use]
    pub fn is_progress(&self) -> bool {
        matches!(
            self.kind,
            DiscoveryKind::WatermarkImprovement
                | DiscoveryKind::FrontierAdvance
                | DiscoveryKind::BucketQuality
        )
    }
}

thread_local! {
    /// Discovery events recorded during the current run.
    static JOURNAL: RefCell<Vec<DiscoveryEvent>> = const { RefCell::new(Vec::new()) };

    /// Hook returning the simulation RNG call count (set by the runner).
    static RNG_COUNT_HOOK: Cell<fn() -> u64> = const { Cell::new(|| 0) };
}

/// Register the function that reports the simulation's RNG call count.
///
/// Must be called by the simulation runner before exploration starts. Forked
/// workers inherit the hook via thread-local storage.
pub fn set_rng_count_hook(get_count: fn() -> u64) {
    RNG_COUNT_HOOK.with(|c| c.set(get_count));
}

/// Install the discovery hook that records events into this journal.
pub(crate) fn install_hooks() {
    fn on_discovery(kind: DiscoveryKind, state_id: u64) {
        let call_count = RNG_COUNT_HOOK.with(Cell::get)();
        JOURNAL.with(|j| {
            let mut journal = j.borrow_mut();
            if journal.len() < MAX_JOURNAL_ENTRIES {
                journal.push(DiscoveryEvent {
                    call_count,
                    kind,
                    state_id,
                });
            }
        });
    }
    moonpool_assertions::set_discovery_hooks(moonpool_assertions::DiscoveryHooks { on_discovery });
}

/// Clear the journal before a run.
pub(crate) fn clear() {
    JOURNAL.with(|j| j.borrow_mut().clear());
}

/// Take the recorded events, leaving the journal empty.
pub(crate) fn take() -> Vec<DiscoveryEvent> {
    JOURNAL.with(|j| std::mem::take(&mut *j.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_records_and_clears() {
        install_hooks();
        set_rng_count_hook(|| 7);
        clear();

        // Simulate the accounting layer signalling a discovery.
        moonpool_assertions::set_discovery_hooks(moonpool_assertions::DiscoveryHooks {
            on_discovery: |_, _| {},
        });
        install_hooks();
        let hooks_event = DiscoveryEvent {
            call_count: 7,
            kind: DiscoveryKind::SometimesPass,
            state_id: 99,
        };
        JOURNAL.with(|j| j.borrow_mut().push(hooks_event));

        let events = take();
        assert_eq!(events, vec![hooks_event]);
        assert!(take().is_empty());

        moonpool_assertions::clear_discovery_hooks();
    }

    #[test]
    fn progress_kinds_classified() {
        let mk = |kind| DiscoveryEvent {
            call_count: 0,
            kind,
            state_id: 0,
        };
        assert!(!mk(DiscoveryKind::SometimesPass).is_progress());
        assert!(!mk(DiscoveryKind::BucketFirst).is_progress());
        assert!(mk(DiscoveryKind::WatermarkImprovement).is_progress());
        assert!(mk(DiscoveryKind::FrontierAdvance).is_progress());
        assert!(mk(DiscoveryKind::BucketQuality).is_progress());
    }
}
