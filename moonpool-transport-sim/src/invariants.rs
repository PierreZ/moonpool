//! Hash-chain integrity invariant.
//!
//! Replays the server-side timeline of [`AppendBlockEvent`]s starting from
//! `(0, INITIAL_DIGEST)` and asserts every event satisfies the chain rule
//! `n == last_n + 1` and `h == fold(last_h, &block)`. Catches transport bugs
//! that reorder, drop, or duplicate server-side events — bugs the workload's
//! per-response reference-model check cannot see on its own.

use std::cell::Cell;

use moonpool_sim::{
    Invariant, SIM_FAULT_TIMELINE, SimFaultEvent, TimelineQuery, TimelineQueryExt, assert_always,
    assert_sometimes,
};

use crate::hash::{INITIAL_DIGEST, fold};
use crate::process::{AppendBlockEvent, TL_APPEND};

/// Replay-based invariant over the server's append timeline.
pub struct TransportIntegrityInvariant {
    cursor: Cell<usize>,
    cursor_faults: Cell<usize>,
    last_n: Cell<u64>,
    last_h: Cell<u64>,
    any_faults: Cell<bool>,
    any_progress: Cell<bool>,
}

impl Default for TransportIntegrityInvariant {
    fn default() -> Self {
        Self::new()
    }
}

impl TransportIntegrityInvariant {
    /// Create a new invariant with empty state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cursor: Cell::new(0),
            cursor_faults: Cell::new(0),
            last_n: Cell::new(0),
            last_h: Cell::new(INITIAL_DIGEST),
            any_faults: Cell::new(false),
            any_progress: Cell::new(false),
        }
    }
}

impl Invariant for TransportIntegrityInvariant {
    fn name(&self) -> &'static str {
        "transport_integrity"
    }

    fn observe(&self, q: &dyn TimelineQuery, _sim_time_ms: u64) {
        let new_entries = q.since::<AppendBlockEvent>(TL_APPEND, &self.cursor);
        for entry in &new_entries {
            let expected_n = self.last_n.get() + 1;
            let expected_h = fold(self.last_h.get(), &entry.event.block);
            assert_always!(entry.event.n == expected_n, "invariant_n_matches_replay");
            assert_always!(entry.event.h == expected_h, "invariant_h_matches_replay");
            self.last_n.set(entry.event.n);
            self.last_h.set(entry.event.h);
            self.any_progress.set(true);
        }

        if !self.any_faults.get() {
            let new_faults = q.since::<SimFaultEvent>(SIM_FAULT_TIMELINE, &self.cursor_faults);
            if !new_faults.is_empty() {
                self.any_faults.set(true);
            }
        }

        assert_sometimes!(
            self.any_faults.get() && self.any_progress.get(),
            "progress_under_transport_chaos"
        );
    }

    fn reset(&mut self) {
        self.cursor.set(0);
        self.cursor_faults.set(0);
        self.last_n.set(0);
        self.last_h.set(INITIAL_DIGEST);
        self.any_faults.set(false);
        self.any_progress.set(false);
    }
}
