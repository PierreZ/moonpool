//! Canonical leader-election example: detect split-brain via captured traces.
//!
//! Demonstrates the end-to-end story for moonpool's plain-`tracing` capture
//! convention:
//!
//! 1. Define a correctness-fact payload (`LeaderElected`) with the required
//!    derives.
//! 2. Workloads emit the fact via `SimContext::emit` (which expands to
//!    `tracing::info!(capture = true, trail = "leader", event = valuable(&..))`).
//! 3. Register a `SplitBrainInvariant` on the builder; it scans the `"leader"`
//!    trail after every captured event and asserts that no two distinct
//!    nodes claim leadership for the same term.
//!
//! Happy path: only one leader per term. Invariant passes; the simulation
//! reports zero failed runs.
//!
//! Bug path: two distinct nodes claim leadership for the same term. The
//! invariant calls `assert_always!`, which records a failure; the simulation
//! reports `failed_runs > 0`.

use std::cell::Cell;
use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use valuable::Valuable;

use moonpool_sim::{
    Invariant, SimContext, SimulationBuilder, SimulationResult, TimeProvider, TrailQuery,
    TrailQueryExt, Workload, assert_always,
};

/// Trail name shared by emitter and invariant.
const LEADER_TRAIL: &str = "leader";

/// Correctness fact emitted on every successful leader election.
///
/// Uses a struct (not a unit-variant enum) so it round-trips cleanly through
/// `valuable-serde` and `serde`.
#[derive(Debug, Clone, Valuable, Serialize, Deserialize)]
struct LeaderElected {
    term: u64,
    leader: String,
}

/// Invariant: at most one distinct leader per term. Panics via
/// `assert_always!` on conflict.
struct SplitBrainInvariant {
    cursor: Cell<usize>,
    /// `term -> first leader observed`. State persists across `observe` calls
    /// within a seed; cleared by `reset()` at the start of each seed.
    seen: std::cell::RefCell<HashMap<u64, String>>,
}

impl SplitBrainInvariant {
    fn new() -> Self {
        Self {
            cursor: Cell::new(0),
            seen: std::cell::RefCell::new(HashMap::new()),
        }
    }
}

impl Invariant for SplitBrainInvariant {
    fn name(&self) -> &'static str {
        "no_split_brain"
    }

    fn observe(&self, q: &dyn TrailQuery, _sim_time_ms: u64) {
        let new_entries = q.since::<LeaderElected>(LEADER_TRAIL, &self.cursor);
        let mut seen = self.seen.borrow_mut();
        for entry in new_entries {
            let term = entry.event.term;
            let leader = entry.event.leader.clone();
            if let Some(prior) = seen.get(&term) {
                assert_always!(
                    prior == &leader,
                    format!("split-brain at term {term}: {prior} vs {leader}")
                );
            } else {
                seen.insert(term, leader);
            }
        }
    }

    fn reset(&mut self) {
        self.cursor.set(0);
        self.seen.borrow_mut().clear();
    }
}

/// Workload emitting `LeaderElected` events. `bug_split_brain` controls
/// whether the workload also emits a *conflicting* leader for term 1, which
/// the invariant must catch.
struct LeaderWorkload {
    bug_split_brain: bool,
}

#[async_trait]
impl Workload for LeaderWorkload {
    fn name(&self) -> &'static str {
        "leader_workload"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Elect node1 across a few terms with small delays so the layer
        // captures them in order with distinct sim times.
        for term in 1..=3 {
            ctx.emit(
                LEADER_TRAIL,
                LeaderElected {
                    term,
                    leader: "node1".into(),
                },
            );
            ctx.time().sleep(Duration::from_millis(10)).await.ok();
        }

        if self.bug_split_brain {
            // Conflict: claim node2 as leader for term 1.
            ctx.emit(
                LEADER_TRAIL,
                LeaderElected {
                    term: 1,
                    leader: "node2".into(),
                },
            );
            ctx.time().sleep(Duration::from_millis(10)).await.ok();
        }

        Ok(())
    }
}

#[test]
fn happy_path_no_split_brain() {
    let report = SimulationBuilder::new()
        .workload(LeaderWorkload {
            bug_split_brain: false,
        })
        .invariant(SplitBrainInvariant::new())
        .set_iterations(3)
        .set_debug_seeds(vec![1, 2, 3])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "happy path should not trip the invariant"
    );
    assert_eq!(report.successful_runs, 3);
}

#[test]
fn split_brain_is_detected() {
    let report = SimulationBuilder::new()
        .workload(LeaderWorkload {
            bug_split_brain: true,
        })
        .invariant(SplitBrainInvariant::new())
        .set_iterations(1)
        .set_debug_seeds(vec![42])
        .run();

    assert!(
        report.failed_runs >= 1,
        "split-brain should produce at least one failed run; got report: {report:?}"
    );
}
