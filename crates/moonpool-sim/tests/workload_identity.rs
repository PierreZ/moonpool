//! Regression: a task that loses its owned workload must stop the campaign.
//!
//! Instance workloads are reused between seeds. A panicked or cancelled phase
//! task consumes its `Box<dyn Workload>`, so continuing would compact the
//! surviving workloads and reuse one under another workload's identity.

use std::future;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use moonpool_sim::{SimContext, SimulationBuilder, SimulationError, SimulationResult, Workload};

static RUN_SURVIVOR_CALLS: AtomicUsize = AtomicUsize::new(0);
static CHECK_SURVIVOR_CALLS: AtomicUsize = AtomicUsize::new(0);
static CHECK_STALL_SURVIVOR_CALLS: AtomicUsize = AtomicUsize::new(0);
static SETUP_SURVIVOR_CALLS: AtomicUsize = AtomicUsize::new(0);
static ERROR_SURVIVOR_CALLS: AtomicUsize = AtomicUsize::new(0);

struct RunPanics;

#[async_trait]
impl Workload for RunPanics {
    fn name(&self) -> &'static str {
        "run_panics"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        panic!("run phase loses the workload instance");
    }
}

struct CheckPanics;

#[async_trait]
impl Workload for CheckPanics {
    fn name(&self) -> &'static str {
        "check_panics"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }

    async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        panic!("check phase loses the workload instance");
    }
}

struct CheckStalls;

#[async_trait]
impl Workload for CheckStalls {
    fn name(&self) -> &'static str {
        "check_stalls"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }

    async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        future::pending::<()>().await;
        Ok(())
    }
}

struct SetupPanics;

#[async_trait]
impl Workload for SetupPanics {
    fn name(&self) -> &'static str {
        "setup_panics"
    }

    async fn setup(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        panic!("setup phase loses the workload instance");
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }
}

struct RunIdentitySurvivor;

#[async_trait]
impl Workload for RunIdentitySurvivor {
    fn name(&self) -> &'static str {
        "run_identity_survivor"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        assert_eq!(ctx.my_ip().to_string(), "10.0.0.2");
        RUN_SURVIVOR_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct CheckIdentitySurvivor;

#[async_trait]
impl Workload for CheckIdentitySurvivor {
    fn name(&self) -> &'static str {
        "check_identity_survivor"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        assert_eq!(ctx.my_ip().to_string(), "10.0.0.2");
        CHECK_SURVIVOR_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct SetupIdentitySurvivor;

#[async_trait]
impl Workload for SetupIdentitySurvivor {
    fn name(&self) -> &'static str {
        "setup_identity_survivor"
    }

    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        assert_eq!(ctx.my_ip().to_string(), "10.0.0.2");
        SETUP_SURVIVOR_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }
}

struct CheckStallIdentitySurvivor;

#[async_trait]
impl Workload for CheckStallIdentitySurvivor {
    fn name(&self) -> &'static str {
        "check_stall_identity_survivor"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        assert_eq!(ctx.my_ip().to_string(), "10.0.0.2");
        CHECK_STALL_SURVIVOR_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct ReturnsError;

#[async_trait]
impl Workload for ReturnsError {
    fn name(&self) -> &'static str {
        "returns_error"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Err(SimulationError::InvalidState(
            "ordinary workload error".to_string(),
        ))
    }
}

struct ErrorIdentitySurvivor;

#[async_trait]
impl Workload for ErrorIdentitySurvivor {
    fn name(&self) -> &'static str {
        "error_identity_survivor"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        assert_eq!(ctx.my_ip().to_string(), "10.0.0.2");
        ERROR_SURVIVOR_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn assert_stops_after_lost_instance(report: &moonpool_sim::SimulationReport, calls: usize) {
    assert_eq!(report.iterations, 1, "report: {report:?}");
    assert_eq!(report.failed_runs, 1, "report: {report:?}");
    assert_eq!(report.seeds_failing, vec![1], "report: {report:?}");
    assert_eq!(calls, 1, "the survivor must not run under another slot");
}

#[test]
fn run_panic_stops_before_reusing_a_survivor() {
    RUN_SURVIVOR_CALLS.store(0, Ordering::SeqCst);
    let report = SimulationBuilder::new()
        .workload(RunPanics)
        .workload(RunIdentitySurvivor)
        .set_iterations(2)
        .set_debug_seeds(vec![1, 2])
        .run()
        .expect("simulation configuration is valid");

    assert_stops_after_lost_instance(&report, RUN_SURVIVOR_CALLS.load(Ordering::SeqCst));
}

#[test]
fn check_panic_stops_before_reusing_a_survivor() {
    CHECK_SURVIVOR_CALLS.store(0, Ordering::SeqCst);
    let report = SimulationBuilder::new()
        .workload(CheckPanics)
        .workload(CheckIdentitySurvivor)
        .set_iterations(2)
        .set_debug_seeds(vec![1, 2])
        .run()
        .expect("simulation configuration is valid");

    assert_stops_after_lost_instance(&report, CHECK_SURVIVOR_CALLS.load(Ordering::SeqCst));
}

#[test]
fn check_stall_cancellation_stops_before_reusing_a_survivor() {
    CHECK_STALL_SURVIVOR_CALLS.store(0, Ordering::SeqCst);
    let report = SimulationBuilder::new()
        .workload(CheckStalls)
        .workload(CheckStallIdentitySurvivor)
        .set_iterations(2)
        .set_debug_seeds(vec![1, 2])
        .run()
        .expect("simulation configuration is valid");

    assert_stops_after_lost_instance(&report, CHECK_STALL_SURVIVOR_CALLS.load(Ordering::SeqCst));
}

#[test]
fn setup_panic_stops_before_reusing_a_survivor() {
    SETUP_SURVIVOR_CALLS.store(0, Ordering::SeqCst);
    let report = SimulationBuilder::new()
        .workload(SetupPanics)
        .workload(SetupIdentitySurvivor)
        .set_iterations(2)
        .set_debug_seeds(vec![1, 2])
        .run()
        .expect("simulation configuration is valid");

    assert_stops_after_lost_instance(&report, SETUP_SURVIVOR_CALLS.load(Ordering::SeqCst));
}

#[test]
fn ordinary_error_returns_instances_for_later_seeds() {
    ERROR_SURVIVOR_CALLS.store(0, Ordering::SeqCst);
    let report = SimulationBuilder::new()
        .workload(ReturnsError)
        .workload(ErrorIdentitySurvivor)
        .set_iterations(2)
        .set_debug_seeds(vec![1, 2])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.iterations, 2, "report: {report:?}");
    assert_eq!(report.failed_runs, 2, "report: {report:?}");
    assert_eq!(ERROR_SURVIVOR_CALLS.load(Ordering::SeqCst), 2);
}
