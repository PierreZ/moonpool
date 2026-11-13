//! Stale process lifecycle events (#457): a kill or a restart decided for
//! one boot of a process must not hit a later boot, and must not end a
//! hold-down.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    FaultContext, FaultInjector, Process, RebootKind, SimContext, SimulationBuilder,
    SimulationError, SimulationResult, TimeProvider, Workload,
};

/// A process that counts its boots, then waits for its shutdown token.
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

/// Sleeps 10 s, then returns.
struct TenSeconds;

#[async_trait]
impl Workload for TenSeconds {
    fn name(&self) -> &'static str {
        "ten_seconds"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.time()
            .sleep(Duration::from_secs(10))
            .await
            .map_err(|error| SimulationError::InvalidState(error.to_string()))
    }
}

/// One step of a script: what to do to process 0.
#[derive(Clone, Copy)]
enum Step {
    /// A graceful reboot with a 4 s grace period and a 1 ms recovery.
    GracefulReboot,
    /// A crash reboot with a 5 s recovery.
    CrashReboot,
    /// A hold-down crash.
    HoldDown,
    /// An explicit restart.
    Restart,
}

/// Runs `script` on process 0: each step after its delay from the start.
struct Script {
    script: Vec<(Duration, Step)>,
}

#[async_trait]
impl FaultInjector for Script {
    fn name(&self) -> &'static str {
        "script"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let ip = ctx.process_ips()[0].clone();
        let start = ctx.time().now();
        for (at, step) in self.script.clone() {
            let wait = (start + at).saturating_sub(ctx.time().now());
            ctx.time()
                .sleep(wait)
                .await
                .map_err(|error| SimulationError::InvalidState(error.to_string()))?;
            match step {
                Step::GracefulReboot => {
                    ctx.reboot_with_delays(&ip, RebootKind::Graceful, &(1..2), &(4000..4001))?;
                }
                Step::CrashReboot => {
                    ctx.reboot_with_delays(&ip, RebootKind::Crash, &(5000..5001), &(1..2))?;
                }
                Step::HoldDown => ctx.crash(&ip)?,
                Step::Restart => ctx.restart(&ip)?,
            }
        }
        Ok(())
    }
}

/// Run `script` on one process and return how many times it booted.
fn boots_after(script: Vec<(Duration, Step)>) -> usize {
    let boots = Arc::new(AtomicUsize::new(0));
    let process_boots = Arc::clone(&boots);
    let report = SimulationBuilder::new()
        .processes(1, move || {
            Box::new(CountedIdle {
                boots: Arc::clone(&process_boots),
            })
        })
        .workload(TenSeconds)
        .fault_factory(move || {
            Box::new(Script {
                script: script.clone(),
            })
        })
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(1)
        .set_debug_seeds(vec![42])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{:?}", report.seeds_failing);
    boots.load(Ordering::SeqCst)
}

/// The issue's case: a restart during a grace period boots the process
/// again, and the grace-period kill at 4 s must not kill that new boot.
#[test]
fn a_stale_grace_period_kill_spares_the_new_boot() {
    let boots = boots_after(vec![
        (Duration::ZERO, Step::GracefulReboot),
        (Duration::from_secs(1), Step::Restart),
    ]);
    assert_eq!(boots, 2, "the first boot and the restart, no third boot");
}

/// A hold-down crash after a crash reboot: the recovery restart of the
/// reboot must not end the hold-down.
#[test]
fn a_pending_recovery_restart_does_not_end_a_hold_down() {
    let boots = boots_after(vec![
        (Duration::ZERO, Step::CrashReboot),
        (Duration::from_secs(1), Step::HoldDown),
    ]);
    assert_eq!(boots, 1, "the process stays down: no restart boots it");
}

/// An explicit restart after a crash reboot: the recovery restart of the
/// reboot must not restart the healthy new boot in place.
#[test]
fn a_pending_recovery_restart_spares_an_explicit_restart() {
    let boots = boots_after(vec![
        (Duration::ZERO, Step::CrashReboot),
        (Duration::from_secs(1), Step::Restart),
    ]);
    assert_eq!(boots, 2, "the first boot and the explicit restart");
}

/// The usual chain still runs: a graceful reboot kills after its grace
/// period and restarts after its recovery delay.
#[test]
fn a_graceful_reboot_still_restarts() {
    let boots = boots_after(vec![(Duration::ZERO, Step::GracefulReboot)]);
    assert_eq!(boots, 2, "the first boot and the restart after the grace");
}
