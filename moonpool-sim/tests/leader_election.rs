//! Canonical leader-election example: detect split-brain via captured traces.
//!
//! Demonstrates the end-to-end story for moonpool's trace-based
//! cross-validation:
//!
//! 1. Workloads emit a plain `tracing` event — exactly the instrumentation
//!    production observability would consume:
//!    `tracing::info!(term, leader = %ip, "leader_elected")`.
//! 2. The simulation captures every such event into a timeline, attributing
//!    its `source` from the actor span the orchestrator wraps around each
//!    task.
//! 3. A `SplitBrainInvariant` registered on the builder scans the
//!    `"leader_elected"` events after every simulation step and asserts that
//!    no two distinct nodes claim leadership for the same term — the same
//!    query you would run against production traces to find a dual leader.
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

use moonpool_sim::{
    Invariant, SimContext, SimulationBuilder, SimulationResult, TimeProvider, TraceQuery, Workload,
    assert_always,
};

/// Event name shared by emitter and invariant.
const LEADER_ELECTED: &str = "leader_elected";

/// Invariant: at most one distinct leader per term. Records a failure via
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

    fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
        let mut seen = self.seen.borrow_mut();
        for e in q.since(LEADER_ELECTED, &self.cursor) {
            let term = e.u64("term").expect("leader_elected carries a term");
            let leader = e
                .str("leader")
                .expect("leader_elected carries a leader")
                .to_owned();
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

/// Workload emitting `leader_elected` events. `bug_split_brain` controls
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
        // Elect node1 across a few terms with small delays so the timeline
        // captures them in order with distinct sim times.
        for term in 1..=3_u64 {
            tracing::info!(term, leader = %"node1", "leader_elected");
            ctx.time().sleep(Duration::from_millis(10)).await.ok();
        }

        if self.bug_split_brain {
            // Conflict: claim node2 as leader for term 1.
            tracing::info!(term = 1_u64, leader = %"node2", "leader_elected");
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
