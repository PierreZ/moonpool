//! Reboot tests for process lifecycle management.
//!
//! Tests that server processes can be killed and restarted while
//! workloads (test drivers) survive reboots.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    Attrition, FaultContext, FaultInjector, NetworkProvider, PhaseConfig, Process, RebootKind,
    SimContext, SimulationBuilder, SimulationResult, TcpListenerTrait, TimeProvider, Workload,
};

// ============================================================================
// Test process: simple echo server that binds and waits for shutdown
// ============================================================================

struct EchoProcess;

#[async_trait(?Send)]
impl Process for EchoProcess {
    fn name(&self) -> &str {
        "echo"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        tracing::debug!("EchoProcess bound to {}", ctx.my_ip());

        loop {
            let result = tokio::select! {
                r = listener.accept() => r,
                _ = ctx.shutdown().cancelled() => return Ok(()),
            };

            match result {
                Ok((mut stream, _)) => {
                    // Simple echo: read and write back
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 64];
                    match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => {
                            let _ = stream.write_all(&buf[..n]).await;
                        }
                        _ => {}
                    }
                }
                Err(e) => {
                    tracing::debug!("EchoProcess accept error (expected under chaos): {}", e);
                }
            }
        }
    }
}

// ============================================================================
// Test workload: client that verifies processes are alive
// ============================================================================

struct ProcessMonitorWorkload;

#[async_trait(?Send)]
impl Workload for ProcessMonitorWorkload {
    fn name(&self) -> &str {
        "monitor"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Verify we can see process IPs in topology
        let process_ips = ctx.topology().all_process_ips();
        assert!(
            !process_ips.is_empty(),
            "workload should see process IPs in topology"
        );

        // Wait for shutdown
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

// ============================================================================
// Test: basic process boot + workload sees process IPs
// ============================================================================

#[test]
fn test_process_boot_and_topology() {
    let report = SimulationBuilder::new()
        .processes(3, || Box::new(EchoProcess))
        .workload(ProcessMonitorWorkload)
        .set_iterations(1)
        .set_debug_seeds(vec![42])
        .run();

    assert_eq!(report.successful_runs, 1, "simulation should succeed");
    assert_eq!(report.failed_runs, 0);
}

// ============================================================================
// Test: process boot with tags
// ============================================================================

struct TagVerifierWorkload;

#[async_trait(?Send)]
impl Workload for TagVerifierWorkload {
    fn name(&self) -> &str {
        "tag_verifier"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Verify process IPs are visible
        let process_ips = ctx.topology().all_process_ips();
        assert_eq!(process_ips.len(), 4, "should have 4 processes");

        // Verify tag queries work
        let east_ips = ctx.topology().ips_tagged("dc", "east");
        let west_ips = ctx.topology().ips_tagged("dc", "west");
        assert_eq!(east_ips.len(), 2, "should have 2 east processes");
        assert_eq!(west_ips.len(), 2, "should have 2 west processes");

        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

#[test]
fn test_process_tags_round_robin() {
    let report = SimulationBuilder::new()
        .processes(4, || Box::new(EchoProcess))
        .tags(&[("dc", &["east", "west"])])
        .workload(TagVerifierWorkload)
        .set_iterations(1)
        .set_debug_seeds(vec![42])
        .run();

    assert_eq!(report.successful_runs, 1, "tag verification should succeed");
}

// ============================================================================
// Test: manual reboot via fault injector
// ============================================================================

struct RebootOnceInjector;

#[async_trait(?Send)]
impl FaultInjector for RebootOnceInjector {
    fn name(&self) -> &str {
        "reboot_once"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        // Wait a bit, then reboot a random process
        let _ = ctx.time().sleep(Duration::from_millis(100)).await;

        if !ctx.chaos_shutdown().is_cancelled() {
            let rebooted = ctx.reboot_random(RebootKind::Crash)?;
            if let Some(ip) = rebooted {
                tracing::info!("Rebooted process at {}", ip);
            }
        }

        // Wait for chaos shutdown
        ctx.chaos_shutdown().cancelled().await;
        Ok(())
    }
}

struct WaitForShutdownWorkload;

#[async_trait(?Send)]
impl Workload for WaitForShutdownWorkload {
    fn name(&self) -> &str {
        "waiter"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

#[test]
fn test_manual_reboot_via_fault_injector() {
    let report = SimulationBuilder::new()
        .processes(3, || Box::new(EchoProcess))
        .workload(WaitForShutdownWorkload)
        .fault(RebootOnceInjector)
        .phases(PhaseConfig {
            chaos_duration: Duration::from_secs(5),
            recovery_duration: Duration::from_secs(5),
        })
        .set_iterations(3)
        .set_debug_seeds(vec![42, 123, 999])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "all iterations should succeed after reboot + recovery"
    );
}

// ============================================================================
// Test: built-in attrition
// ============================================================================

#[test]
fn test_builtin_attrition() {
    let report = SimulationBuilder::new()
        .processes(3, || Box::new(EchoProcess))
        .workload(WaitForShutdownWorkload)
        .attrition(Attrition {
            max_dead: 1,
            prob_graceful: 0.3,
            prob_crash: 0.5,
            prob_wipe: 0.2,
            recovery_delay_ms: None,
            grace_period_ms: None,
        })
        .phases(PhaseConfig {
            chaos_duration: Duration::from_secs(10),
            recovery_duration: Duration::from_secs(10),
        })
        .set_iterations(3)
        .set_debug_seeds(vec![42, 123, 999])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "attrition should not cause workload failures"
    );
}

// ============================================================================
// Test: tag-based reboot
// ============================================================================

struct RebootTaggedInjector;

#[async_trait(?Send)]
impl FaultInjector for RebootTaggedInjector {
    fn name(&self) -> &str {
        "reboot_tagged"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let _ = ctx.time().sleep(Duration::from_millis(100)).await;

        if !ctx.chaos_shutdown().is_cancelled() {
            let rebooted = ctx.reboot_tagged("dc", "east", RebootKind::Crash)?;
            tracing::info!("Rebooted {} east processes", rebooted.len());
        }

        ctx.chaos_shutdown().cancelled().await;
        Ok(())
    }
}

#[test]
fn test_tag_based_reboot() {
    let report = SimulationBuilder::new()
        .processes(4, || Box::new(EchoProcess))
        .tags(&[("dc", &["east", "west"])])
        .workload(WaitForShutdownWorkload)
        .fault(RebootTaggedInjector)
        .phases(PhaseConfig {
            chaos_duration: Duration::from_secs(5),
            recovery_duration: Duration::from_secs(10),
        })
        .set_iterations(1)
        .set_debug_seeds(vec![42])
        .run();

    assert_eq!(report.failed_runs, 0);
}

// ============================================================================
// Test: process reads its own tags via SimContext
// ============================================================================

struct TagAwareProcess;

#[async_trait(?Send)]
impl Process for TagAwareProcess {
    fn name(&self) -> &str {
        "tag_aware"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Verify this process has tags
        let my_tags = ctx.topology().my_tags();
        let role = my_tags.get("role");
        tracing::info!("Process at {} has role={:?}", ctx.my_ip(), role);

        // All processes should have a role tag
        if role.is_none() {
            return Err(moonpool_sim::SimulationError::InvalidState(
                "process should have a role tag".into(),
            ));
        }

        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

#[test]
fn test_process_reads_own_tags() {
    let report = SimulationBuilder::new()
        .processes(3, || Box::new(TagAwareProcess))
        .tags(&[("role", &["leader", "follower"])])
        .workload(WaitForShutdownWorkload)
        .set_iterations(1)
        .set_debug_seeds(vec![42])
        .run();

    assert_eq!(report.successful_runs, 1);
}

// ============================================================================
// Test: graceful reboot signals shutdown token — process exits cleanly
// ============================================================================

/// Process that detects shutdown token cancellation and exits cleanly.
struct GracefulProcess;

#[async_trait(?Send)]
impl Process for GracefulProcess {
    fn name(&self) -> &str {
        "graceful"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        tracing::info!("GracefulProcess bound to {}", ctx.my_ip());

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((mut stream, _)) => {
                            use tokio::io::{AsyncReadExt, AsyncWriteExt};
                            let mut buf = [0u8; 64];
                            if let Ok(n) = stream.read(&mut buf).await {
                                if n > 0 {
                                    let _ = stream.write_all(&buf[..n]).await;
                                }
                            }
                        }
                        Err(_) => {}
                    }
                }
                _ = ctx.shutdown().cancelled() => {
                    tracing::info!("GracefulProcess at {} saw shutdown, exiting cleanly", ctx.my_ip());
                    return Ok(());
                }
            }
        }
    }
}

/// Fault injector that triggers a single graceful reboot.
struct GracefulRebootInjector;

#[async_trait(?Send)]
impl FaultInjector for GracefulRebootInjector {
    fn name(&self) -> &str {
        "graceful_reboot"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let _ = ctx.time().sleep(Duration::from_millis(100)).await;

        if !ctx.chaos_shutdown().is_cancelled() {
            let rebooted = ctx.reboot_random(RebootKind::Graceful)?;
            if let Some(ip) = rebooted {
                tracing::info!("Initiated graceful reboot for {}", ip);
            }
        }

        ctx.chaos_shutdown().cancelled().await;
        Ok(())
    }
}

#[test]
fn test_graceful_reboot_signals_shutdown_token() {
    let report = SimulationBuilder::new()
        .processes(3, || Box::new(GracefulProcess))
        .workload(WaitForShutdownWorkload)
        .fault(GracefulRebootInjector)
        .phases(PhaseConfig {
            chaos_duration: Duration::from_secs(10),
            recovery_duration: Duration::from_secs(10),
        })
        .set_iterations(3)
        .set_debug_seeds(vec![42, 123, 999])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "graceful reboot should succeed — process exits cleanly on shutdown signal"
    );
}

// ============================================================================
// Test: graceful reboot force-kills stuck process after grace period
// ============================================================================

/// Process that ignores the shutdown token and loops forever.
struct StuckProcess;

#[async_trait(?Send)]
impl Process for StuckProcess {
    fn name(&self) -> &str {
        "stuck"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let _listener = ctx.network().bind(ctx.my_ip()).await?;
        tracing::info!("StuckProcess bound to {}", ctx.my_ip());

        // Deliberately ignore ctx.shutdown() — loop forever until force-killed
        loop {
            let _ = ctx.time().sleep(Duration::from_secs(1)).await;
        }
    }
}

#[test]
fn test_graceful_reboot_force_kills_stuck_process() {
    let report = SimulationBuilder::new()
        .processes(3, || Box::new(StuckProcess))
        .workload(WaitForShutdownWorkload)
        .fault(GracefulRebootInjector)
        .phases(PhaseConfig {
            chaos_duration: Duration::from_secs(10),
            recovery_duration: Duration::from_secs(10),
        })
        .set_iterations(3)
        .set_debug_seeds(vec![42, 123, 999])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "stuck process should be force-killed after grace period and restarted"
    );
}

// ============================================================================
// Test: max_dead limits concurrent kills
// ============================================================================

#[test]
fn test_max_dead_limits_concurrent_kills_via_attrition() {
    // Use built-in attrition with max_dead=1 — the AttritionInjector
    // respects dead_count and won't kill more than 1 at a time
    let report = SimulationBuilder::new()
        .processes(5, || Box::new(EchoProcess))
        .workload(WaitForShutdownWorkload)
        .attrition(Attrition {
            max_dead: 1,
            prob_graceful: 0.0,
            prob_crash: 1.0,
            prob_wipe: 0.0,
            recovery_delay_ms: Some(500..2000),
            grace_period_ms: None,
        })
        .phases(PhaseConfig {
            chaos_duration: Duration::from_secs(10),
            recovery_duration: Duration::from_secs(10),
        })
        .set_iterations(5)
        .set_debug_seeds(vec![42, 123, 999, 7, 314])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "attrition with max_dead=1 should not cause failures"
    );
}
