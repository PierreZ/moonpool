//! The runner's verdict on fault injectors (#453): an injector that returns
//! `Err` fails the seed, and the end of the run (the `Shutdown` event that
//! cuts a sleep) is not an error of the built-in injectors.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, FaultContext, FaultInjector,
    Process, SimContext, SimulationBuilder, SimulationResult, TimeProvider, Workload,
};

/// A process that waits for shutdown.
struct Idle;

#[async_trait]
impl Process for Idle {
    fn name(&self) -> &'static str {
        "idle"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

/// Sleeps `duration`, and returns `Ok` also when the runner's shutdown cuts
/// the sleep.
struct Sleeps {
    name: &'static str,
    duration: Duration,
}

#[async_trait]
impl Workload for Sleeps {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let _cut = ctx.time().sleep(self.duration).await;
        Ok(())
    }
}

/// Fails at once, as an injector that targets an unknown IP does.
struct FailingInjector;

#[async_trait]
impl FaultInjector for FailingInjector {
    fn name(&self) -> &'static str {
        "failing"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        ctx.crash("not-an-ip")
    }
}

/// A custom injector that returns `Err` fails the seed: it ran less chaos
/// than its author asked for.
#[test]
fn a_failing_injector_fails_the_seed() {
    let report = SimulationBuilder::new()
        .processes(1, || Box::new(Idle))
        .workload(Sleeps {
            name: "sleeps",
            duration: Duration::from_secs(1),
        })
        .fault_factory(|| Box::new(FailingInjector))
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(1)
        .set_debug_seeds(vec![42])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(
        report.failed_runs, 1,
        "the injector's error must fail the seed"
    );
    assert_eq!(report.seeds_failing, vec![42]);
}

/// The first workload to return shuts the run down, and the `Shutdown` event
/// cuts the attrition injector's sleep. That is the end of the run, not an
/// injector error: the seed passes.
#[test]
fn shutdown_does_not_fail_the_builtin_injectors() {
    let report = SimulationBuilder::new()
        .processes(3, || Box::new(Idle))
        .workload(Sleeps {
            name: "returns_soon",
            duration: Duration::from_secs(1),
        })
        .workload(Sleeps {
            name: "returns_late",
            duration: Duration::from_secs(10),
        })
        .enable_chaos([Chaos::Attrition {
            config: Attrition {
                max_dead: 1,
                prob_graceful: 0.0,
                prob_crash: 1.0,
                prob_wipe: 0.0,
                recovery_delay_ms: None,
                grace_period_ms: None,
                scope: AttritionScope::PerProcess,
                victims: AttritionVictims::Any,
                wipe_budget: None,
            },
            mode: ChaosMode::Random,
        }])
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(3)
        .set_debug_seeds(vec![1, 2, 3])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(
        report.failed_runs, 0,
        "a cut sleep is the end of the run: {:?}",
        report.seeds_failing
    );
}
