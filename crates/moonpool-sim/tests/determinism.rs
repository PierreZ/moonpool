//! End-to-end determinism guard: the same seed must replay the exact same
//! execution, event for event, across full `SimulationBuilder` runs.
//!
//! This is the permanent tripwire for entropy leaks anywhere in the stack
//! (executor scheduling, select! offsets, providers): if any component
//! consults a non-seeded randomness source, the recorded traces diverge and
//! this test fails.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    SimContext, SimulationBuilder, SimulationResult, TaskProvider, TimeProvider, Workload,
};

/// Process-wide execution trace, rebuilt per run.
static TRACE: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record(entry: String) {
    TRACE
        .lock()
        .expect("Mutex poisoned: prior task panicked")
        .push(entry);
}

/// Spawns racing tasks that interleave sleeps and yields, recording every
/// step with its virtual timestamp.
struct RacingWorkload {
    label: &'static str,
}

#[async_trait]
impl Workload for RacingWorkload {
    fn name(&self) -> &'static str {
        self.label
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let mut handles = Vec::new();
        for i in 0..4u32 {
            let time = ctx.time().clone();
            let task = ctx.task().yield_now();
            drop(task); // exercise the provider surface without awaiting
            let label = self.label;
            handles.push(
                ctx.task()
                    .spawn_task(&format!("{label}-racer-{i}"), async move {
                        for round in 0..3u32 {
                            // Identical deadlines wake the racers in the same steps;
                            // recording order captures the executor's decisions.
                            let _ = time.sleep(Duration::from_millis(10)).await;
                            record(format!("{label} task{i} round{round} t={:?}", time.now()));
                        }
                    }),
            );
        }
        for handle in handles {
            let _ = handle.await;
        }
        record(format!("{} done t={:?}", self.label, ctx.time().now()));
        Ok(())
    }
}

fn run_once(seed: u64) -> Vec<String> {
    TRACE
        .lock()
        .expect("Mutex poisoned: prior task panicked")
        .clear();
    let report = SimulationBuilder::new()
        .workload(RacingWorkload { label: "alpha" })
        .workload(RacingWorkload { label: "beta" })
        .set_debug_seeds(vec![seed])
        // Exactly one iteration: the default UntilCoverageStable keeps
        // iterating past the debug seed, which is not what a trace-equality
        // check wants.
        .set_iterations(1)
        .run();
    assert_eq!(report.failed_runs, 0, "seed {seed} must succeed");
    TRACE
        .lock()
        .expect("Mutex poisoned: prior task panicked")
        .clone()
}

#[test]
fn same_seed_produces_identical_execution_traces() {
    let first = run_once(42);
    let second = run_once(42);
    assert!(!first.is_empty(), "the workloads must have recorded steps");
    assert_eq!(
        first, second,
        "same seed must replay the exact same execution"
    );
}

#[test]
fn other_seeds_also_complete() {
    let trace = run_once(7);
    assert!(!trace.is_empty());
}
