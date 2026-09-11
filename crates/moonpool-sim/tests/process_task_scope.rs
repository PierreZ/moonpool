//! Regression: a crashed process's spawned tasks die with it.
//!
//! A process spawns a background worker through `ctx.task().spawn_task()`,
//! drops the join handle, and parks. Crashing the process used to abort only
//! the root future: the task provider was stateless and spawned straight into
//! the shared executor, so the worker kept running through the hold-down and
//! beside the rebooted process's own worker afterwards — a "crash" that
//! preserved application work and in-memory state, which no recovery test
//! can trust.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    FaultContext, FaultInjector, Process, SimContext, SimulationBuilder, SimulationError,
    SimulationResult, TaskProvider, TimeProvider, Workload, assert_always,
};

/// Spawns a detached worker that bumps a per-IP counter every 10ms, then
/// parks until shut down. All of its work happens in the child task.
struct WorkerProcess;

#[async_trait]
impl Process for WorkerProcess {
    fn name(&self) -> &'static str {
        "worker_process"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let key = format!("worker_ticks:{}", ctx.my_ip());
        let time = ctx.time().clone();
        let state = ctx.state().clone();
        ctx.task()
            .spawn_task("worker", async move {
                loop {
                    if time.sleep(Duration::from_millis(10)).await.is_err() {
                        return;
                    }
                    let ticks: u64 = state.get(&key).unwrap_or(0);
                    state.publish(&key, ticks + 1);
                }
            })
            .detach();
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

/// Keeps the run alive long enough for the script to play out.
struct PatientWorkload;

#[async_trait]
impl Workload for PatientWorkload {
    fn name(&self) -> &'static str {
        "patient"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        for _ in 0..200 {
            ctx.time()
                .sleep(Duration::from_millis(5))
                .await
                .map_err(|e| SimulationError::InvalidState(format!("sleep failed: {e}")))?;
        }
        Ok(())
    }
}

/// Crash process 0, hold it down, and require its worker to have stopped;
/// then restart it and require a worker to be ticking again.
struct CrashAndWatchWorker;

impl CrashAndWatchWorker {
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
impl FaultInjector for CrashAndWatchWorker {
    fn name(&self) -> &'static str {
        "crash_and_watch_worker"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        if !Self::pause(ctx, 100).await? {
            return Ok(());
        }
        let target = ctx.process_ips()[0].clone();
        let key = format!("worker_ticks:{target}");
        assert_always!(
            ctx.state().get::<u64>(&key).unwrap_or(0) > 0,
            "the worker ticked before the crash"
        );

        ctx.crash(&target)?;
        if !Self::pause(ctx, 30).await? {
            return Ok(());
        }
        let held: u64 = ctx.state().get(&key).unwrap_or(0);
        if !Self::pause(ctx, 200).await? {
            return Ok(());
        }
        assert_always!(
            ctx.state().get::<u64>(&key).unwrap_or(0) == held,
            "a crashed process's spawned tasks must stop with it"
        );

        ctx.restart(&target)?;
        let mut recovered = false;
        for _ in 0..20 {
            if !Self::pause(ctx, 20).await? {
                break;
            }
            if ctx.state().get::<u64>(&key).unwrap_or(0) > held {
                recovered = true;
                break;
            }
        }
        assert_always!(recovered, "the restarted process's worker ticks again");
        Ok(())
    }
}

#[test]
fn a_crash_stops_the_tasks_the_process_spawned() {
    let report = SimulationBuilder::new()
        .processes(1, || Box::new(WorkerProcess))
        .workload_factory(|| Box::new(PatientWorkload))
        .fault_factory(|| Box::new(CrashAndWatchWorker))
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(3)
        .set_debug_seeds(vec![7, 11, 13])
        .run();

    assert_eq!(report.iterations, 3);
    assert_eq!(
        report.failed_runs, 0,
        "no spawned task may outlive its process's crash"
    );
    assert_eq!(report.successful_runs, 3);
}
