//! Per-process clocks (`Chaos::Clocks`, `ClockModel`).
//!
//! Each process boot reads its own `now()`, which starts near zero, and each
//! machine's clock runs at its own rate. Production clocks behave this way;
//! one world clock for all processes hides the bugs that come from it.

use std::{net::IpAddr, time::Duration};

use async_trait::async_trait;
use moonpool_sim::{
    Chaos, ChaosMode, ClockModel, Event, FaultContext, FaultInjector, Process, SimContext,
    SimTimeProvider, SimWorld, SimulationBuilder, SimulationError, SimulationResult, TimeProvider,
    Workload, assert_always,
};

const LAST_NOW: &str = "clock:last_now";
const WENT_BACK: &str = "clock:went_back";

fn sleep_error(error: impl std::fmt::Display) -> SimulationError {
    SimulationError::InvalidState(format!("sleep failed: {error}"))
}

/// Persists its `now()` every 10 ms. At boot, it compares its `now()`
/// with the value that the previous boot persisted.
struct PersistingProcess;

#[async_trait]
impl Process for PersistingProcess {
    fn name(&self) -> &'static str {
        "persisting"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let now = ctx.time().now();
        if let Some(persisted) = ctx.state().get::<Duration>(LAST_NOW) {
            ctx.state().publish(WENT_BACK, now < persisted);
        }
        while !ctx.shutdown().is_cancelled() {
            ctx.state().publish(LAST_NOW, ctx.time().now());
            if ctx.time().sleep(Duration::from_millis(10)).await.is_err() {
                break;
            }
        }
        Ok(())
    }
}

/// Lets the process run for 3 s, crashes it and restarts it.
struct CrashOnce;

#[async_trait]
impl FaultInjector for CrashOnce {
    fn name(&self) -> &'static str {
        "crash_once"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let target = ctx.process_ips()[0].clone();
        ctx.time()
            .sleep(Duration::from_secs(3))
            .await
            .map_err(sleep_error)?;
        ctx.crash(&target)?;
        ctx.time()
            .sleep(Duration::from_millis(50))
            .await
            .map_err(sleep_error)?;
        ctx.restart(&target)
    }
}

/// Waits until the restarted process compared its clock.
struct ClockWatcher;

#[async_trait]
impl Workload for ClockWatcher {
    fn name(&self) -> &'static str {
        "clock_watcher"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.time()
            .sleep(Duration::from_secs(4))
            .await
            .map_err(sleep_error)?;
        assert_always!(
            ctx.state().get::<bool>(WENT_BACK) == Some(true),
            "a process reads a smaller now() after a reboot"
        );
        Ok(())
    }
}

#[test]
fn a_persisted_now_is_larger_than_the_now_after_a_reboot() {
    let report = SimulationBuilder::new()
        .processes(1, || Box::new(PersistingProcess))
        .workload_factory(|| Box::new(ClockWatcher))
        .fault_factory(|| Box::new(CrashOnce))
        .enable_chaos([Chaos::Clocks(ChaosMode::Random)])
        .chaos_duration(Duration::from_secs(5))
        .set_iterations(3)
        .set_debug_seeds(vec![1, 2, 3])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report:?}");
    assert_eq!(report.successful_runs, 3);
}

/// Move world time to `at` with one timer event.
fn advance_to(sim: &mut SimWorld, at: Duration) {
    sim.schedule_event_at(Event::Timer { task_id: u64::MAX }, at);
    while sim.has_pending_events() {
        sim.step();
    }
}

#[test]
fn two_processes_read_time_at_different_rates() {
    let mut sim = SimWorld::new();
    let model = ClockModel::new(Duration::ZERO, ClockModel::MAX_RATE_ERROR_PPM, 42);
    sim.set_clock_model(Some(model));
    let first: IpAddr = "10.0.1.1".parse().expect("valid address");
    let second: IpAddr = "10.0.1.2".parse().expect("valid address");
    sim.boot_process_clock(first);
    sim.boot_process_clock(second);
    let rate_of = |sim: &SimWorld, ip| {
        sim.process_clock(ip)
            .expect("the process has a clock")
            .rate_error_ppm
    };
    let rates = [rate_of(&sim, first), rate_of(&sim, second)];
    assert_ne!(rates[0], rates[1], "salt 42 gives two rates");

    let first_time = SimTimeProvider::for_ip(sim.downgrade(), first);
    let second_time = SimTimeProvider::for_ip(sim.downgrade(), second);
    let workload_time = SimTimeProvider::new(sim.downgrade());
    advance_to(&mut sim, Duration::from_secs(100));

    assert_eq!(workload_time.now(), Duration::from_secs(100));
    let expected = |error: i64| {
        let error = i128::from(error);
        let nanos = 100_000_000_000_i128 * (1_000_000 + error) / 1_000_000;
        Duration::from_nanos(u64::try_from(nanos).expect("in range"))
    };
    assert_eq!(first_time.now(), expected(rates[0]));
    assert_eq!(second_time.now(), expected(rates[1]));
    assert_ne!(first_time.now(), second_time.now());
}

#[test]
fn without_a_model_a_process_reads_world_time() {
    let mut sim = SimWorld::new();
    let ip: IpAddr = "10.0.1.1".parse().expect("valid address");
    sim.boot_process_clock(ip);
    assert_eq!(sim.process_clock(ip), None);
    let time = SimTimeProvider::for_ip(sim.downgrade(), ip);
    advance_to(&mut sim, Duration::from_secs(7));
    assert_eq!(time.now(), Duration::from_secs(7));
}
