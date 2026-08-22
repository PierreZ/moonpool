//! Regression: a self-perpetuating timer must become an actionable deadlock.
//!
//! A downstream user (a Pulsar client driver) hit a non-termination where a
//! **detached** engine task — spawned via the `TaskProvider`, *not* registered
//! as a `Workload` handle — entered a reconnect/keepalive loop that re-armed a
//! `TimeProvider::sleep` timer every tick, advancing virtual time without
//! bound while the owning workload sat blocked on it.
//!
//! The orchestrator never flagged this:
//!
//! - The no-progress `DeadlockDetector` only counts a no-progress iteration
//!   when `pending_event_count() == 0`. The keepalive timer keeps the queue
//!   non-empty forever, so the counter resets every iteration and never trips.
//! - The run-phase loop ends only when every registered workload handle has
//!   finished. A detached task is never in that set, so a workload internally
//!   stuck awaiting it keeps the run alive indefinitely.
//!
//! The fix is a configurable, generous-by-default **virtual-time budget** on
//! the run phase: if simulated time climbs past the budget while workloads are
//! still running, the orchestrator triggers a shutdown and — if time keeps
//! climbing past another full budget — declares a deadlock. The decision is a
//! pure function of the event schedule, so replays stay bit-for-bit identical.

use std::future;
use std::time::Duration;

use async_trait::async_trait;

use moonpool_sim::{
    SimContext, SimulationBuilder, SimulationResult, TaskProvider, TimeProvider, Workload,
};

/// Workload that reproduces the busy-but-non-completing-peer hang.
///
/// `run()` spawns a **detached** keepalive task that re-arms a sleep every
/// tick (the self-perpetuating timer), then blocks forever. It deliberately
/// does *not* honour `ctx.shutdown()`, mirroring the downstream bug where the
/// workload is internally wedged behind a detached task that ignores
/// cancellation. Without the virtual-time budget this run never terminates.
struct SelfPerpetuatingTimerWorkload;

#[async_trait]
impl Workload for SelfPerpetuatingTimerWorkload {
    fn name(&self) -> &'static str {
        "self_perpetuating_timer"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let time = ctx.time().clone();
        // Detached keepalive loop: re-arms a 30s sleep every tick, keeping the
        // event queue non-empty forever and advancing virtual time without
        // bound. Not a registered workload handle, so the orchestrator's
        // "all workloads done" exit never sees it.
        ctx.task().spawn_task("keepalive", async move {
            loop {
                let _ = time.sleep(Duration::from_secs(30)).await;
            }
        });

        // Block forever, ignoring shutdown — the workload is wedged behind the
        // detached task. The virtual-time budget must rescue us.
        future::pending::<()>().await;
        Ok(())
    }
}

/// The detached keepalive timer must be caught by the run-phase virtual-time
/// budget and reported as a failed (deadlocked) run instead of hanging.
#[test]
fn self_perpetuating_timer_is_caught_by_run_time_budget() {
    let report = SimulationBuilder::new()
        .workload(SelfPerpetuatingTimerWorkload)
        // Small budget so the test trips fast. The keepalive advances virtual
        // time +30s/tick, so a 1min budget is breached within a couple of ticks.
        .run_time_budget(Duration::from_mins(1))
        .set_iterations(1)
        .set_debug_seeds(vec![42])
        .run();

    assert!(
        report.failed_runs >= 1,
        "self-perpetuating timer should be reported as a deadlocked (failed) run; got report: {report:?}"
    );
    assert!(
        report.seeds_failing.contains(&42),
        "the failing seed should be surfaced for replay; got report: {report:?}"
    );
}

/// A normal workload that finishes well within the budget must still pass —
/// the budget must not perturb a sim that terminates on its own.
struct WellBehavedWorkload;

#[async_trait]
impl Workload for WellBehavedWorkload {
    fn name(&self) -> &'static str {
        "well_behaved"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        for _ in 0..3 {
            ctx.time().sleep(Duration::from_millis(10)).await.ok();
        }
        Ok(())
    }
}

/// A tiny budget must not affect a workload whose own logic completes quickly:
/// its virtual time stays far below the budget, so the guard never fires.
#[test]
fn well_behaved_workload_is_unaffected_by_budget() {
    let report = SimulationBuilder::new()
        .workload(WellBehavedWorkload)
        // Even a deliberately tiny budget must not trip a sim that finishes
        // inside it (3 x 10ms = 30ms of virtual time, well under 1s).
        .run_time_budget(Duration::from_secs(1))
        .set_iterations(3)
        .set_debug_seeds(vec![1, 2, 3])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "a workload that finishes inside the budget must not be flagged; got report: {report:?}"
    );
    assert_eq!(report.successful_runs, 3);
}
