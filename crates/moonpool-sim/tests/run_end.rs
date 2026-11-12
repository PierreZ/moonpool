//! The end of a run: the run phase stops at the step where the last
//! workload returned (#456), and the settle phase does not fail a healthy
//! seed when the events left are far in simulated time (#455). Every task
//! sees the shutdown before the `Shutdown` event closes the listeners.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    FaultContext, FaultInjector, NetworkProvider, Process, SimContext, SimulationBuilder,
    SimulationError, SimulationResult, TcpListenerTrait, TimeProvider, Workload,
};

/// When the workload returns.
const WORKLOAD_END: Duration = Duration::from_secs(10);

/// A process that counts its boots, then waits for shutdown with no timer.
struct CountedIdle {
    boots: Arc<AtomicUsize>,
}

#[async_trait]
impl Process for CountedIdle {
    fn name(&self) -> &'static str {
        "idle"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        self.boots.fetch_add(1, Ordering::SeqCst);
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

/// Sleeps [`WORKLOAD_END`], then returns.
struct SleepThenReturn;

#[async_trait]
impl Workload for SleepThenReturn {
    fn name(&self) -> &'static str {
        "sleep_then_return"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.time()
            .sleep(WORKLOAD_END)
            .await
            .map_err(|error| SimulationError::InvalidState(error.to_string()))
    }
}

/// Crashes process `i` with a restart after `downs[i]`, then returns.
struct FarRestarts {
    downs: Vec<Duration>,
}

#[async_trait]
impl FaultInjector for FarRestarts {
    fn name(&self) -> &'static str {
        "far_restarts"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        for (ip, down) in ctx.process_ips().iter().zip(&self.downs) {
            ctx.crash_for(ip, *down)?;
        }
        Ok(())
    }
}

fn builder(boots: &Arc<AtomicUsize>, downs: Vec<Duration>) -> SimulationBuilder {
    let boots = Arc::clone(boots);
    let processes = downs.len();
    SimulationBuilder::new()
        .processes(processes, move || {
            Box::new(CountedIdle {
                boots: Arc::clone(&boots),
            })
        })
        .workload_factory(|| Box::new(SleepThenReturn))
        .fault_factory(move || {
            Box::new(FarRestarts {
                downs: downs.clone(),
            })
        })
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(1)
        .set_debug_seeds(vec![42])
}

/// The run phase ends at the step where the last workload returned. A
/// restart scheduled for after that point must not run, and the report's
/// simulated time ends near the return of the workload.
#[test]
fn run_phase_ends_when_the_last_workload_returns() {
    let boots = Arc::new(AtomicUsize::new(0));
    let report = builder(&boots, vec![Duration::from_secs(600)])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.failed_runs, 0, "{:?}", report.seeds_failing);
    assert_eq!(
        boots.load(Ordering::SeqCst),
        1,
        "the restart at 600 s is after the run: the process must not boot again"
    );
    assert!(
        report.metrics.simulated_time < WORKLOAD_END + Duration::from_secs(1),
        "simulated time {:?} must end near the workload's return at {WORKLOAD_END:?}",
        report.metrics.simulated_time
    );
}

/// Restarts far in simulated time after the run do not fail the settle
/// phase: no process is left for them to act on.
#[test]
fn far_restarts_do_not_fail_the_settle_phase() {
    let boots = Arc::new(AtomicUsize::new(0));
    let report = builder(
        &boots,
        vec![
            Duration::from_secs(600),
            Duration::from_mins(20),
            Duration::from_mins(30),
        ],
    )
    .run()
    .expect("simulation configuration is valid");

    assert_eq!(
        report.failed_runs, 0,
        "a healthy seed with far restarts must pass: {:?}",
        report.seeds_failing
    );
    assert!(
        report.metrics.simulated_time < Duration::from_secs(600),
        "settle must not step to the far restarts: {:?}",
        report.metrics.simulated_time
    );
}

/// A server that retries `accept` ahead of its shutdown check, like the
/// `operator` example's node: a `biased` select polls the accept first.
struct AcceptFirst;

#[async_trait]
impl Process for AcceptFirst {
    fn name(&self) -> &'static str {
        "accept_first"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        loop {
            moonpool_sim::select! {
                biased;
                _accepted = listener.accept() => {}
                () = ctx.shutdown().cancelled() => return Ok(()),
            }
        }
    }
}

/// Returns after `after`; a sleep the end of the run cuts is not an error.
struct ReturnAfter {
    name: &'static str,
    after: Duration,
}

#[async_trait]
impl Workload for ReturnAfter {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let _cut = ctx.time().sleep(self.after).await;
        Ok(())
    }
}

/// The first return shuts the run down. Every task sees the cancelled token
/// before the `Shutdown` event closes the listeners, so a server whose
/// `accept` fails at once after that event does not spin forever.
#[test]
fn servers_see_the_shutdown_before_their_listeners_close() {
    let report = SimulationBuilder::new()
        .processes(2, || Box::new(AcceptFirst))
        .workload(ReturnAfter {
            name: "returns_soon",
            after: Duration::from_secs(1),
        })
        .workload(ReturnAfter {
            name: "returns_late",
            after: Duration::from_secs(10),
        })
        .set_iterations(1)
        .set_debug_seeds(vec![42])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.failed_runs, 0, "{:?}", report.seeds_failing);
}
