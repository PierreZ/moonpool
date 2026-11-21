//! A custom providers bundle built with `SimProviders::from_env`.
//!
//! `SimulationBuilder::with_providers` hands each process boot and each
//! workload a bundle of the test's own type. The tasks spawned through that
//! bundle's task provider must behave as the tasks spawned through
//! `ctx.task()`: a process crash stops them, and a panic in one fails the
//! seed. A workload keeps one bundle for its whole iteration, `check()`
//! included.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    FaultContext, FaultInjector, Process, ProviderEnv, Providers, SimContext, SimProviders,
    SimulationBuilder, SimulationError, SimulationResult, TaskProvider, TimeProvider, Workload,
    assert_always,
};

/// A bundle that is not `Providers` itself: the simulated providers plus an
/// id that tells one factory call from another.
#[derive(Clone)]
struct AppProviders {
    sim: SimProviders,
    id: u64,
}

impl AppProviders {
    fn factory() -> impl Fn(&ProviderEnv) -> Self + 'static {
        let next = Arc::new(AtomicU64::new(0));
        move |env| Self {
            sim: SimProviders::from_env(env),
            id: next.fetch_add(1, Ordering::Relaxed),
        }
    }
}

fn sleep_error(error: impl std::fmt::Display) -> SimulationError {
    SimulationError::InvalidState(format!("sleep failed: {error}"))
}

/// Spawns, through its bundle's task provider, a worker that bumps a per-IP
/// counter every 10ms, then parks until shut down.
struct BundleWorkerProcess;

#[async_trait]
impl Process<AppProviders> for BundleWorkerProcess {
    fn name(&self) -> &'static str {
        "bundle_worker"
    }

    async fn run(&mut self, ctx: &SimContext<AppProviders>) -> SimulationResult<()> {
        let key = format!("worker_ticks:{}", ctx.my_ip());
        let time = ctx.providers().sim.time().clone();
        let state = ctx.state().clone();
        ctx.providers()
            .sim
            .task()
            .spawn_task("bundle-worker", async move {
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

/// Keeps the run alive long enough for the fault script to play out.
struct PatientWorkload;

#[async_trait]
impl Workload<AppProviders> for PatientWorkload {
    fn name(&self) -> &'static str {
        "patient"
    }

    async fn run(&mut self, ctx: &SimContext<AppProviders>) -> SimulationResult<()> {
        for _ in 0..200 {
            ctx.providers()
                .sim
                .time()
                .sleep(Duration::from_millis(5))
                .await
                .map_err(sleep_error)?;
        }
        Ok(())
    }
}

/// Crash process 0, hold it down and require its worker to have stopped,
/// then restart it and require a worker to tick again.
struct CrashAndWatchWorker;

impl CrashAndWatchWorker {
    async fn pause(ctx: &FaultContext, ms: u64) -> SimulationResult<bool> {
        if ctx.chaos_shutdown().is_cancelled() {
            return Ok(false);
        }
        ctx.time()
            .sleep(Duration::from_millis(ms))
            .await
            .map_err(sleep_error)?;
        Ok(!ctx.chaos_shutdown().is_cancelled())
    }
}

#[async_trait]
impl FaultInjector for CrashAndWatchWorker {
    fn name(&self) -> &'static str {
        "crash_and_watch_bundle_worker"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        if !Self::pause(ctx, 100).await? {
            return Ok(());
        }
        let target = ctx.process_ips()[0].clone();
        let key = format!("worker_ticks:{target}");
        assert_always!(
            ctx.state().get::<u64>(&key).unwrap_or(0) > 0,
            "the bundle's worker ticked before the crash"
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
            "a task spawned through a crashed process's bundle must stop with it"
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
        assert_always!(
            recovered,
            "the restarted process's bundle worker ticks again"
        );
        Ok(())
    }
}

#[test]
fn a_crash_stops_the_tasks_spawned_through_the_bundle() {
    let report = SimulationBuilder::with_providers(AppProviders::factory())
        .processes(1, || Box::new(BundleWorkerProcess))
        .workload_factory(|| Box::new(PatientWorkload))
        .fault_factory(|| Box::new(CrashAndWatchWorker))
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(3)
        .set_debug_seeds(vec![7, 11, 13])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.iterations, 3);
    assert_eq!(
        report.failed_runs, 0,
        "no task spawned through the bundle may outlive its process's crash: {report:?}"
    );
}

/// Spawns, through its bundle's task provider, a task that panics, then
/// parks until shut down.
struct PanickingBundleChild;

#[async_trait]
impl Process<AppProviders> for PanickingBundleChild {
    fn name(&self) -> &'static str {
        "panicking_bundle_child"
    }

    async fn run(&mut self, ctx: &SimContext<AppProviders>) -> SimulationResult<()> {
        ctx.providers()
            .sim
            .task()
            .spawn_task("bundle-child", async {
                panic!("bundle child panic");
            })
            .detach();
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

#[test]
fn a_panic_in_a_task_spawned_through_the_bundle_fails_the_seed() {
    let report = SimulationBuilder::with_providers(AppProviders::factory())
        .processes(1, || Box::new(PanickingBundleChild))
        .workload_factory(|| Box::new(PatientWorkload))
        .set_iterations(1)
        .set_debug_seeds(vec![1])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.failed_runs, 1, "report: {report:?}");
    assert_eq!(report.seeds_failing, vec![1], "report: {report:?}");
}

/// Records the bundle id `run()` saw and requires `check()` to see the same.
#[derive(Default)]
struct SameBundleWorkload {
    run_id: Option<u64>,
}

#[async_trait]
impl Workload<AppProviders> for SameBundleWorkload {
    fn name(&self) -> &'static str {
        "same_bundle"
    }

    async fn setup(&mut self, ctx: &SimContext<AppProviders>) -> SimulationResult<()> {
        self.run_id = Some(ctx.providers().id);
        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext<AppProviders>) -> SimulationResult<()> {
        if self.run_id == Some(ctx.providers().id) {
            Ok(())
        } else {
            Err(SimulationError::InvalidState(
                "run() got a bundle setup() did not have".to_string(),
            ))
        }
    }

    async fn check(&mut self, ctx: &SimContext<AppProviders>) -> SimulationResult<()> {
        if self.run_id == Some(ctx.providers().id) {
            Ok(())
        } else {
            Err(SimulationError::InvalidState(
                "check() got a bundle run() did not have".to_string(),
            ))
        }
    }
}

#[test]
fn a_workload_keeps_one_bundle_for_its_iteration() {
    let report = SimulationBuilder::with_providers(AppProviders::factory())
        .workload_factory(|| Box::new(SameBundleWorkload::default()))
        .set_iterations(2)
        .set_debug_seeds(vec![1, 2])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.failed_runs, 0, "report: {report:?}");
    assert_eq!(report.successful_runs, 2);
}
