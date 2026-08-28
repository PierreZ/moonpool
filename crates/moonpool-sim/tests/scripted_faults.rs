//! Scripted process lifecycle fault tests.
//!
//! A factory-created fault injector crashes a selected process, holds it down
//! until the workload reaches a deterministic operation milestone (observed
//! through the shared `StateHandle`), then explicitly restarts it — proving
//! the crash / hold-down / restart split on `FaultContext`, `fault_factory`
//! compatibility with in-process exploration, and exact replay.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    FaultContext, FaultInjector, Process, SimContext, SimulationBuilder, SimulationError,
    SimulationResult, TimeProvider, Workload, assert_always,
};

/// Milestone (in workload operations) the crashed process is held down until.
const HOLD_MILESTONE: u64 = 100;
/// Total operations the workload executes; well past the milestone so the
/// injector finishes its script before the run ends.
const TOTAL_OPS: u64 = 600;

/// Timestamped fault-script events, shared across timelines of one run so two
/// identical runs can be compared for exact replay.
type EventLog = Arc<Mutex<Vec<(&'static str, u128)>>>;

fn log_event(events: &EventLog, label: &'static str, now: Duration) {
    events
        .lock()
        .expect("RwLock poisoned: prior task panicked")
        .push((label, now.as_millis()));
}

/// Process that increments a per-IP tick counter in shared state every 10ms.
/// A held-down process must stop ticking.
struct TickerProcess;

#[async_trait]
impl Process for TickerProcess {
    fn name(&self) -> &'static str {
        "ticker"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let key = format!("ticks:{}", ctx.my_ip());
        loop {
            let slept = moonpool_sim::select! {
                biased;
                () = ctx.shutdown().cancelled() => return Ok(()),
                r = ctx.time().sleep(Duration::from_millis(10)) => r,
            };
            slept.map_err(|e| SimulationError::InvalidState(format!("sleep failed: {e}")))?;
            let ticks: u64 = ctx.state().get(&key).unwrap_or(0);
            ctx.state().publish(&key, ticks + 1);
        }
    }
}

/// Workload that executes operations at a fixed rate, publishing the running
/// count so the fault injector can coordinate on a deterministic milestone.
struct CountingWorkload;

#[async_trait]
impl Workload for CountingWorkload {
    fn name(&self) -> &'static str {
        "counter"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        for op in 1..=TOTAL_OPS {
            ctx.time()
                .sleep(Duration::from_millis(5))
                .await
                .map_err(|e| SimulationError::InvalidState(format!("sleep failed: {e}")))?;
            ctx.state().publish("ops", op);
        }
        Ok(())
    }
}

/// Scripted injector: crash process 0, hold it down until the workload passes
/// [`HOLD_MILESTONE`] operations, verify no process work happened while held,
/// then explicitly restart and verify recovery.
struct ScriptedCrashInjector {
    events: EventLog,
}

impl ScriptedCrashInjector {
    /// Sleep briefly, bailing out early when the chaos phase ends.
    async fn pause(ctx: &FaultContext, ms: u64) -> SimulationResult<bool> {
        if ctx.chaos_shutdown().is_cancelled() {
            return Ok(false);
        }
        ctx.time()
            .sleep(Duration::from_millis(ms))
            .await
            .map_err(|e| SimulationError::InvalidState(format!("sleep failed: {e}")))?;
        Ok(!ctx.chaos_shutdown().is_cancelled())
    }
}

#[async_trait]
impl FaultInjector for ScriptedCrashInjector {
    fn name(&self) -> &'static str {
        "scripted_crash"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        // Let processes boot and the workload start making progress.
        if !Self::pause(ctx, 100).await? {
            return Ok(());
        }
        let target = ctx.process_ips()[0].clone();
        let ticks_key = format!("ticks:{target}");

        ctx.crash(&target)?;
        log_event(&self.events, "crash", ctx.time().now());

        // Give the force-kill event time to land, then snapshot the tick
        // counter: it must not move again until the explicit restart.
        if !Self::pause(ctx, 30).await? {
            return Ok(());
        }
        let ticks_held: u64 = ctx.state().get(&ticks_key).unwrap_or(0);

        // Hold the process down until the workload milestone.
        while ctx.state().get::<u64>("ops").unwrap_or(0) < HOLD_MILESTONE {
            if !Self::pause(ctx, 20).await? {
                return Ok(());
            }
        }
        log_event(&self.events, "milestone", ctx.time().now());
        assert_always!(
            ctx.state().get::<u64>(&ticks_key).unwrap_or(0) == ticks_held,
            "held-down process must not run application work"
        );

        ctx.restart(&target)?;
        log_event(&self.events, "restart", ctx.time().now());

        // Recovery: the fresh instance must resume ticking before the chaos
        // cutoff, leaving the workload a quiet convergence tail.
        let mut recovered = false;
        for _ in 0..50 {
            if !Self::pause(ctx, 20).await? {
                break;
            }
            if ctx.state().get::<u64>(&ticks_key).unwrap_or(0) > ticks_held {
                recovered = true;
                break;
            }
        }
        assert_always!(recovered, "restarted process must resume work");
        log_event(&self.events, "recovered", ctx.time().now());
        Ok(())
    }
}

/// Build the scripted-fault simulation against a shared event log.
fn scripted_builder(events: &EventLog) -> SimulationBuilder {
    let events = events.clone();
    SimulationBuilder::new()
        .processes(2, || Box::new(TickerProcess))
        .workload_factory(|| Box::new(CountingWorkload))
        .fault_factory(move || {
            Box::new(ScriptedCrashInjector {
                events: events.clone(),
            })
        })
        .chaos_duration(Duration::from_secs(30))
}

/// The crash → hold-down → milestone → restart script succeeds across seeds:
/// no process work during the hold-down interval, recovery before the cutoff.
#[test]
fn test_scripted_crash_hold_restart() {
    let events: EventLog = Arc::default();
    let report = scripted_builder(&events)
        .set_iterations(3)
        .set_debug_seeds(vec![7, 11, 13])
        .run();
    assert_eq!(report.failed_runs, 0, "scripted lifecycle script must pass");
    assert_eq!(report.successful_runs, 3);

    let log = events
        .lock()
        .expect("RwLock poisoned: prior task panicked")
        .clone();
    let labels: Vec<&str> = log.iter().map(|(label, _)| *label).collect();
    assert_eq!(
        labels,
        [
            "crash",
            "milestone",
            "restart",
            "recovered",
            "crash",
            "milestone",
            "restart",
            "recovered",
            "crash",
            "milestone",
            "restart",
            "recovered",
        ],
        "each seed must run the full script exactly once"
    );
}

/// Two identical fresh-builder runs replay the exact same crash / restart
/// timeline: same event order and identical simulated timestamps.
#[test]
fn test_scripted_faults_replay_exactly() {
    let run = || {
        let events: EventLog = Arc::default();
        let report = scripted_builder(&events)
            .set_iterations(1)
            .set_debug_seeds(vec![4242])
            .run();
        assert_eq!(report.failed_runs, 0);
        events
            .lock()
            .expect("RwLock poisoned: prior task panicked")
            .clone()
    };
    let first = run();
    let second = run();
    assert_eq!(
        first, second,
        "fresh-builder replay must reproduce identical crash/restart events"
    );
    assert_eq!(first.len(), 4, "one full script per run");
}

/// `fault_factory` is accepted by in-process exploration (`workers: 0`): a
/// fresh injector is built for every explored timeline, and two identical
/// exploration runs stay fully deterministic.
#[cfg(feature = "exploration")]
#[test]
fn test_fault_factory_with_in_process_exploration() {
    let run = || {
        let events: EventLog = Arc::default();
        let report = scripted_builder(&events)
            .set_iterations(1)
            .set_debug_seeds(vec![99])
            .enable_exploration(moonpool_sim::ExplorationConfig {
                workers: 0,
                max_runs_per_seed: 4,
                branching_factor: 2,
                max_frontier: 16,
                max_recipe_len: 4,
            })
            .run();
        assert_eq!(report.failed_runs, 0, "exploration run must pass");
        let exploration = report.exploration.expect("exploration report missing");
        let log = events
            .lock()
            .expect("RwLock poisoned: prior task panicked")
            .clone();
        (exploration.total_timelines, log)
    };

    let (timelines_a, log_a) = run();
    let (timelines_b, log_b) = run();
    assert_eq!(timelines_a, timelines_b);
    assert_eq!(
        log_a, log_b,
        "explored timelines must replay identical fault scripts"
    );
    assert!(
        log_a.len() >= 4,
        "the root timeline must complete the script"
    );
}
