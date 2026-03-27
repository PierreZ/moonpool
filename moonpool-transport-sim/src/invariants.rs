//! Delivery contract invariants checked via timelines.
//!
//! Validates cross-operation and cross-workload properties that inline
//! per-operation assertions cannot catch. Runs after every simulation event
//! using incremental timeline scanning.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;

use moonpool_sim::{
    Invariant, SIM_FAULT_TIMELINE, SimFaultEvent, StateHandle, assert_always, assert_sometimes,
};

use crate::workload::{DeliveryEvent, TL_AT_LEAST_ONCE, TL_AT_MOST_ONCE, TL_TIMEOUT};

/// Invariant that validates delivery mode contracts by scanning per-mode timelines.
///
/// Uses cursor-based incremental processing to stay fast (runs after every event).
/// Maintains per-mode sets of sent/resolved seq_ids to detect phantoms and
/// double-resolutions.
pub struct DeliveryContractInvariant {
    // Cursors for incremental timeline scanning
    cursor_amo: Cell<usize>,
    cursor_alo: Cell<usize>,
    cursor_to: Cell<usize>,
    cursor_faults: Cell<usize>,

    // at_most_once tracking
    amo_sent: RefCell<HashSet<u64>>,
    amo_resolved: RefCell<HashSet<u64>>,

    // at_least_once tracking
    alo_sent: RefCell<HashSet<u64>>,

    // timeout tracking
    to_timed_out: RefCell<HashSet<u64>>,

    // cross-mode tracking
    any_faults: Cell<bool>,
    any_replies: Cell<bool>,
}

impl Default for DeliveryContractInvariant {
    fn default() -> Self {
        Self::new()
    }
}

impl DeliveryContractInvariant {
    /// Create a new invariant with empty state.
    pub fn new() -> Self {
        Self {
            cursor_amo: Cell::new(0),
            cursor_alo: Cell::new(0),
            cursor_to: Cell::new(0),
            cursor_faults: Cell::new(0),
            amo_sent: RefCell::new(HashSet::new()),
            amo_resolved: RefCell::new(HashSet::new()),
            alo_sent: RefCell::new(HashSet::new()),
            to_timed_out: RefCell::new(HashSet::new()),
            any_faults: Cell::new(false),
            any_replies: Cell::new(false),
        }
    }

    /// Check at-most-once timeline: no phantoms, at most one resolution per seq_id.
    fn check_at_most_once(&self, state: &StateHandle) {
        let Some(tl) = state.timeline::<DeliveryEvent>(TL_AT_MOST_ONCE) else {
            return;
        };
        let len = tl.len();
        let cursor = self.cursor_amo.get();
        if len == cursor {
            return;
        }

        let new_entries = tl.since(cursor);
        self.cursor_amo.set(len);

        let mut sent = self.amo_sent.borrow_mut();
        let mut resolved = self.amo_resolved.borrow_mut();

        for entry in &new_entries {
            match &entry.event {
                DeliveryEvent::Sent { seq_id, .. } => {
                    sent.insert(*seq_id);
                }
                DeliveryEvent::Replied { seq_id } => {
                    assert_always!(sent.contains(seq_id), "amo_no_phantom_reply");
                    assert_always!(!resolved.contains(seq_id), "amo_at_most_one_resolution");
                    resolved.insert(*seq_id);
                    self.any_replies.set(true);
                }
                DeliveryEvent::MaybeDelivered { seq_id } => {
                    assert_always!(!resolved.contains(seq_id), "amo_at_most_one_resolution");
                    assert_sometimes!(true, "amo_invariant_maybe_delivered_observed");
                    resolved.insert(*seq_id);
                }
                DeliveryEvent::Failed { seq_id, .. } => {
                    assert_always!(!resolved.contains(seq_id), "amo_at_most_one_resolution");
                    resolved.insert(*seq_id);
                }
                DeliveryEvent::TimedOut { .. } => {}
            }
        }
    }

    /// Check at-least-once timeline: no phantom replies.
    fn check_at_least_once(&self, state: &StateHandle) {
        let Some(tl) = state.timeline::<DeliveryEvent>(TL_AT_LEAST_ONCE) else {
            return;
        };
        let len = tl.len();
        let cursor = self.cursor_alo.get();
        if len == cursor {
            return;
        }

        let new_entries = tl.since(cursor);
        self.cursor_alo.set(len);

        let mut sent = self.alo_sent.borrow_mut();

        for entry in &new_entries {
            match &entry.event {
                DeliveryEvent::Sent { seq_id, .. } => {
                    sent.insert(*seq_id);
                }
                DeliveryEvent::Replied { seq_id } => {
                    assert_always!(sent.contains(seq_id), "alo_no_phantom_reply");
                    assert_sometimes!(true, "alo_invariant_reply_observed");
                    self.any_replies.set(true);
                }
                _ => {}
            }
        }
    }

    /// Check timeout timeline: no response after timeout declared.
    fn check_timeout(&self, state: &StateHandle) {
        let Some(tl) = state.timeline::<DeliveryEvent>(TL_TIMEOUT) else {
            return;
        };
        let len = tl.len();
        let cursor = self.cursor_to.get();
        if len == cursor {
            return;
        }

        let new_entries = tl.since(cursor);
        self.cursor_to.set(len);

        let mut timed_out = self.to_timed_out.borrow_mut();

        for entry in &new_entries {
            match &entry.event {
                DeliveryEvent::TimedOut { seq_id, .. } => {
                    assert_sometimes!(true, "timeout_invariant_timeout_observed");
                    timed_out.insert(*seq_id);
                }
                DeliveryEvent::Replied { seq_id } => {
                    assert_always!(
                        !timed_out.contains(seq_id),
                        "timeout_no_reply_after_timeout"
                    );
                    self.any_replies.set(true);
                }
                _ => {}
            }
        }
    }

    /// Check cross-mode: faults injected AND messages still delivered.
    fn check_cross_mode(&self, state: &StateHandle) {
        if !self.any_faults.get() {
            let Some(tl) = state.timeline::<SimFaultEvent>(SIM_FAULT_TIMELINE) else {
                return;
            };
            let len = tl.len();
            let cursor = self.cursor_faults.get();
            if len > cursor {
                self.cursor_faults.set(len);
                self.any_faults.set(true);
            }
        }

        assert_sometimes!(
            self.any_faults.get() && self.any_replies.get(),
            "cross_mode_recovery_after_faults"
        );
    }
}

impl Invariant for DeliveryContractInvariant {
    fn name(&self) -> &str {
        "delivery_contract"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        self.check_at_most_once(state);
        self.check_at_least_once(state);
        self.check_timeout(state);
        self.check_cross_mode(state);
    }

    fn reset(&mut self) {
        self.cursor_amo.set(0);
        self.cursor_alo.set(0);
        self.cursor_to.set(0);
        self.cursor_faults.set(0);
        self.amo_sent.borrow_mut().clear();
        self.amo_resolved.borrow_mut().clear();
        self.alo_sent.borrow_mut().clear();
        self.to_timed_out.borrow_mut().clear();
        self.any_faults.set(false);
        self.any_replies.set(false);
    }
}
