//! The runner's verdicts on the end of a run (#382): a sleep that the
//! `Shutdown` event cuts reports an error, and `check()` runs with chaos
//! ended in every mode.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use moonpool_sim::{
    SimContext, SimulationBuilder, SimulationError, SimulationResult, TaskProvider, TimeError,
    TimeProvider, Workload,
};

/// Returns after one second, so the runner shuts down its sibling while
/// the sibling sleeps.
struct ReturnsSoon;

#[async_trait]
impl Workload for ReturnsSoon {
    fn name(&self) -> &'static str {
        "returns_soon"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.time()
            .sleep(Duration::from_secs(1))
            .await
            .map_err(|error| SimulationError::InvalidState(error.to_string()))
    }
}

/// Sleeps for one hour and records what the sleep returned.
struct LongSleeper {
    outcome: Arc<Mutex<Option<Result<(), TimeError>>>>,
}

#[async_trait]
impl Workload for LongSleeper {
    fn name(&self) -> &'static str {
        "long_sleeper"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // A ticker keeps events between the sibling's return and the long
        // timer, so the `Shutdown` event lands before the timer fires.
        let time = ctx.time().clone();
        ctx.task().spawn_task("ticker", async move {
            while time.sleep(Duration::from_millis(100)).await.is_ok() {}
        });
        let result = ctx.time().sleep(Duration::from_hours(1)).await;
        *self.outcome.lock().expect("lock") = Some(result);
        Ok(())
    }
}

/// When the first workload returns, the runner sends the `Shutdown` event.
/// A sleep that it cuts did not last its full duration, so it must return
/// `Err(TimeError::Shutdown)`, not `Ok(())`.
#[test]
fn a_sleep_cut_by_shutdown_returns_shutdown() {
    let outcome = Arc::new(Mutex::new(None));
    let report = SimulationBuilder::new()
        .workload(ReturnsSoon)
        .workload(LongSleeper {
            outcome: Arc::clone(&outcome),
        })
        .set_iterations(1)
        .set_debug_seeds(vec![7])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.failed_runs, 0, "{report:?}");
    let outcome = outcome.lock().expect("lock").take();
    assert!(
        matches!(outcome, Some(Err(TimeError::Shutdown))),
        "a cut sleep must return Shutdown, got {outcome:?}"
    );
}

/// Returns before its long chaos duration elapses, then checks that chaos
/// ended before `check()`.
struct EarlyReturnQuietCheck;

#[async_trait]
impl Workload for EarlyReturnQuietCheck {
    fn name(&self) -> &'static str {
        "early_return"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }

    async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        if moonpool_sim::in_chaos() {
            return Err(SimulationError::InvalidState(
                "check() ran inside the chaos phase".to_string(),
            ));
        }
        Ok(())
    }
}

/// `check()` runs with no faults also when every workload returned before
/// `chaos_duration` elapsed (the default mode).
#[test]
fn check_is_quiet_when_workloads_return_before_chaos_ends() {
    let report = SimulationBuilder::new()
        .workload(EarlyReturnQuietCheck)
        .chaos_duration(Duration::from_hours(1))
        .set_iterations(1)
        .set_debug_seeds(vec![11])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.failed_runs, 0, "{report:?}");
}

/// The same with no chaos duration at all.
#[test]
fn check_is_quiet_with_no_chaos_duration() {
    let report = SimulationBuilder::new()
        .workload(EarlyReturnQuietCheck)
        .set_iterations(1)
        .set_debug_seeds(vec![12])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.failed_runs, 0, "{report:?}");
}
