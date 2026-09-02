//! Reboot tests for process lifecycle management.
//!
//! Tests that server processes can be killed and restarted while
//! workloads (test drivers) survive reboots.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, FaultContext, FaultInjector,
    Invariant, NetworkProvider, Process, RebootKind, SIM_FAULT_EVENT_NAME, SimContext,
    SimulationBuilder, SimulationResult, TcpListenerTrait, TimeProvider, TraceQuery, Workload,
    assert_always,
};

use std::cell::Cell;

// ============================================================================
// Test process: simple echo server that binds and waits for shutdown
// ============================================================================

struct EchoProcess;

#[async_trait]
impl Process for EchoProcess {
    fn name(&self) -> &'static str {
        "echo"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        tracing::debug!("EchoProcess bound to {}", ctx.my_ip());

        loop {
            let result = moonpool_sim::select! {
                biased;
                r = listener.accept() => r,
                () = ctx.shutdown().cancelled() => return Ok(()),
            };

            match result {
                Ok((mut stream, _)) => {
                    // Simple echo: read and write back
                    use futures::io::{AsyncReadExt, AsyncWriteExt};
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

#[async_trait]
impl Workload for ProcessMonitorWorkload {
    fn name(&self) -> &'static str {
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

#[async_trait]
impl Workload for TagVerifierWorkload {
    fn name(&self) -> &'static str {
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
        .expect("tags after processes")
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

#[async_trait]
impl FaultInjector for RebootOnceInjector {
    fn name(&self) -> &'static str {
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

/// Workload that runs for a fixed sim-time duration then returns.
struct TimedWorkload(Duration);

#[async_trait]
impl Workload for TimedWorkload {
    fn name(&self) -> &'static str {
        "timed"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.time().sleep(self.0).await.map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("sleep failed: {e}"))
        })?;
        Ok(())
    }
}

#[test]
fn test_manual_reboot_via_fault_injector() {
    let report = SimulationBuilder::new()
        .processes(3, || Box::new(EchoProcess))
        .workload(TimedWorkload(Duration::from_secs(15)))
        .fault(RebootOnceInjector)
        .chaos_duration(Duration::from_secs(5))
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
        .workload(TimedWorkload(Duration::from_secs(25)))
        .enable_chaos([Chaos::Attrition {
            config: Attrition {
                max_dead: 1,
                prob_graceful: 0.3,
                prob_crash: 0.5,
                prob_wipe: 0.2,
                recovery_delay_ms: None,
                grace_period_ms: None,
                scope: AttritionScope::PerProcess,
                victims: AttritionVictims::Any,
            },
            mode: ChaosMode::Random,
        }])
        .chaos_duration(Duration::from_secs(10))
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

#[async_trait]
impl FaultInjector for RebootTaggedInjector {
    fn name(&self) -> &'static str {
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
        .expect("tags after processes")
        .workload(TimedWorkload(Duration::from_secs(20)))
        .fault(RebootTaggedInjector)
        .chaos_duration(Duration::from_secs(5))
        .set_iterations(1)
        .set_debug_seeds(vec![42])
        .run();

    assert_eq!(report.failed_runs, 0);
}

// ============================================================================
// Test: process reads its own tags via SimContext
// ============================================================================

struct TagAwareProcess;

#[async_trait]
impl Process for TagAwareProcess {
    fn name(&self) -> &'static str {
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
        .expect("tags after processes")
        .workload(TimedWorkload(Duration::from_secs(1)))
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

#[async_trait]
impl Process for GracefulProcess {
    fn name(&self) -> &'static str {
        "graceful"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        tracing::info!("GracefulProcess bound to {}", ctx.my_ip());

        loop {
            moonpool_sim::select! {
                biased;
                result = listener.accept() => {
                    if let Ok((mut stream, _)) = result {
                        use futures::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 64];
                        if let Ok(n) = stream.read(&mut buf).await
                            && n > 0 {
                            let _ = stream.write_all(&buf[..n]).await;
                        }
                    }
                }
                () = ctx.shutdown().cancelled() => {
                    tracing::info!("GracefulProcess at {} saw shutdown, exiting cleanly", ctx.my_ip());
                    return Ok(());
                }
            }
        }
    }
}

/// Fault injector that triggers a single graceful reboot.
struct GracefulRebootInjector;

#[async_trait]
impl FaultInjector for GracefulRebootInjector {
    fn name(&self) -> &'static str {
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
        .workload(TimedWorkload(Duration::from_secs(25)))
        .fault(GracefulRebootInjector)
        .chaos_duration(Duration::from_secs(10))
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

#[async_trait]
impl Process for StuckProcess {
    fn name(&self) -> &'static str {
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
        .workload(TimedWorkload(Duration::from_secs(25)))
        .fault(GracefulRebootInjector)
        .chaos_duration(Duration::from_secs(10))
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

// ============================================================================
// Reboot timing invariant: validates grace period and event ordering
// ============================================================================

/// Invariant that validates process lifecycle timing from the fault timeline.
///
/// Checks after every simulation step:
/// - `process_graceful_shutdown` → `process_force_kill`: delta == `grace_period_ms`
/// - `process_force_kill` → `process_restart`: delta > 0 (recovery delay is positive)
/// - Events for same IP appear in correct order
struct RebootTimingInvariant {
    last_checked: Cell<usize>,
}

impl RebootTimingInvariant {
    fn new() -> Self {
        Self {
            last_checked: Cell::new(0),
        }
    }
}

impl Invariant for RebootTimingInvariant {
    fn name(&self) -> &'static str {
        "reboot_timing"
    }

    fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
        let len = q.len(SIM_FAULT_EVENT_NAME);
        if len == self.last_checked.get() {
            return; // No new events
        }
        self.last_checked.set(len);

        let entries = q.snapshot(SIM_FAULT_EVENT_NAME);

        // Check grace period timing: graceful shutdown → force kill for same IP.
        // Crash force-kills carry a different cause and have no grace period.
        for (i, entry) in entries.iter().enumerate() {
            if entry.str("kind") == Some("process_force_kill")
                && entry.str("cause") == Some("grace_period_expired")
            {
                let ip = entry.str("ip").expect("force kill carries an ip");
                // Look backwards for matching graceful shutdown
                for j in (0..i).rev() {
                    if entries[j].str("kind") == Some("process_graceful_shutdown")
                        && entries[j].str("ip") == Some(ip)
                    {
                        let grace_period_ms = entries[j]
                            .u64("grace_period_ms")
                            .expect("graceful shutdown carries grace_period_ms");
                        let actual_delta = entry.time_ms - entries[j].time_ms;
                        assert_always!(
                            actual_delta == grace_period_ms,
                            format!(
                                "Grace period mismatch for {}: expected {}ms, got {}ms",
                                ip, grace_period_ms, actual_delta
                            )
                        );
                        break;
                    }
                }
            }
        }

        // Check ordering: force kill → restart for same IP
        for (i, entry) in entries.iter().enumerate() {
            if entry.str("kind") == Some("process_restart") {
                let ip = entry.str("ip").expect("restart carries an ip");
                for j in (0..i).rev() {
                    if entries[j].str("kind") == Some("process_force_kill")
                        && entries[j].str("ip") == Some(ip)
                    {
                        assert_always!(
                            entry.time_ms > entries[j].time_ms,
                            format!(
                                "process_restart at {}ms should be after process_force_kill at {}ms for {}",
                                entry.time_ms, entries[j].time_ms, ip
                            )
                        );
                        break;
                    }
                }
            }
        }
    }

    fn reset(&mut self) {
        self.last_checked.set(0);
    }
}

#[test]
fn test_graceful_reboot_timing_invariant() {
    let report = SimulationBuilder::new()
        .processes(3, || Box::new(GracefulProcess))
        .workload(TimedWorkload(Duration::from_secs(25)))
        .fault(GracefulRebootInjector)
        .invariant(RebootTimingInvariant::new())
        .chaos_duration(Duration::from_secs(10))
        .set_iterations(3)
        .set_debug_seeds(vec![42, 123, 999])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "graceful reboot timing invariant should pass"
    );
}

#[test]
fn test_attrition_timing_invariant() {
    let report = SimulationBuilder::new()
        .processes(3, || Box::new(EchoProcess))
        .workload(TimedWorkload(Duration::from_secs(25)))
        .enable_chaos([Chaos::Attrition {
            config: Attrition {
                max_dead: 1,
                prob_graceful: 0.5,
                prob_crash: 0.3,
                prob_wipe: 0.2,
                recovery_delay_ms: Some(500..2000),
                grace_period_ms: Some(1000..3000),
                scope: AttritionScope::PerProcess,
                victims: AttritionVictims::Any,
            },
            mode: ChaosMode::Random,
        }])
        .invariant(RebootTimingInvariant::new())
        .chaos_duration(Duration::from_secs(10))
        .set_iterations(5)
        .set_debug_seeds(vec![42, 123, 999, 7, 314])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "attrition with timing invariant should pass"
    );
}

#[test]
fn test_max_dead_limits_concurrent_kills_via_attrition() {
    // Use built-in attrition with max_dead=1 — the AttritionInjector
    // respects dead_count and won't kill more than 1 at a time
    let report = SimulationBuilder::new()
        .processes(5, || Box::new(EchoProcess))
        .workload(TimedWorkload(Duration::from_secs(25)))
        .enable_chaos([Chaos::Attrition {
            config: Attrition {
                max_dead: 1,
                prob_graceful: 0.0,
                prob_crash: 1.0,
                prob_wipe: 0.0,
                recovery_delay_ms: Some(500..2000),
                grace_period_ms: None,
                scope: AttritionScope::PerProcess,
                victims: AttritionVictims::Any,
            },
            mode: ChaosMode::Random,
        }])
        .chaos_duration(Duration::from_secs(10))
        .set_iterations(5)
        .set_debug_seeds(vec![42, 123, 999, 7, 314])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "attrition with max_dead=1 should not cause failures"
    );
}

// ============================================================================
// Test: a crash reboot silences the process at the crash instant
// ============================================================================
//
// Regression guard for the crash path scheduling only `ProcessRestart`: the
// old task was aborted inside `ProcessManager::restart`, i.e. *after* the
// recovery delay, so a crashed process kept running application work
// throughout the interval it was reported dead.

/// Event name emitted by [`HeartbeatProcess`] on every tick.
const HEARTBEAT: &str = "process_heartbeat";

/// Sim-time between two heartbeats.
const HEARTBEAT_PERIOD: Duration = Duration::from_millis(200);

/// Fixed recovery delay for [`CrashOnceInjector`], in milliseconds. Spans many
/// heartbeat periods so a task that survives the crash cannot stay silent by
/// accident.
const CRASH_RECOVERY_MS: usize = 3000;

/// Process that ticks forever. A crashed instance must go silent immediately
/// instead of ticking on until its restart.
struct HeartbeatProcess;

#[async_trait]
impl Process for HeartbeatProcess {
    fn name(&self) -> &'static str {
        "heartbeat"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let mut beat = 0u64;
        while !ctx.shutdown().is_cancelled() {
            ctx.time().sleep(HEARTBEAT_PERIOD).await.map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("sleep failed: {e}"))
            })?;
            beat += 1;
            tracing::info!(beat, "process_heartbeat");
        }
        Ok(())
    }
}

/// Crashes the first process once, with a fixed recovery delay so kill and
/// restart times are comparable across replays of the same seed.
///
/// It records the IP and the sim time of the crash itself, so the invariant
/// knows when the dead interval starts without relying on the very fault event
/// this test is about.
struct CrashOnceInjector {
    observations: SharedObservations,
}

#[async_trait]
impl FaultInjector for CrashOnceInjector {
    fn name(&self) -> &'static str {
        "crash_once"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let _ = ctx.time().sleep(Duration::from_secs(1)).await;

        if let Some(ip) = ctx.process_ips().first().cloned() {
            let crashed_at_ms = u64::try_from(ctx.time().now().as_millis())
                .expect("sim time fits in u64 milliseconds");
            ctx.reboot_with_delays(
                &ip,
                RebootKind::Crash,
                &(CRASH_RECOVERY_MS..CRASH_RECOVERY_MS + 1),
                &(0..1),
            )?;
            lock(&self.observations).crashed = Some((ip, crashed_at_ms));
        }

        ctx.chaos_shutdown().cancelled().await;
        Ok(())
    }
}

/// Shared observations for one run of the crash scenario.
#[derive(Default)]
struct CrashObservations {
    /// `(ip, sim time in ms)` of the crash, recorded by the fault injector.
    crashed: Option<(String, u64)>,
    /// `kind@ip@time_ms` for every process-lifecycle fault, in order. Two runs
    /// of the same seed must produce identical vectors.
    lifecycle: Vec<String>,
    /// Number of crash → restart windows proven silent end to end.
    verified_windows: usize,
}

type SharedObservations = std::sync::Arc<std::sync::Mutex<CrashObservations>>;

fn lock(observations: &SharedObservations) -> std::sync::MutexGuard<'_, CrashObservations> {
    observations
        .lock()
        .expect("CrashObservations poisoned: prior task panicked")
}

/// Invariant: from the instant a process is crashed until it restarts, that
/// process emits nothing.
///
/// The window start comes from the injector's own record of the crash, not
/// from a simulator fault event, so the invariant still fires if the crash
/// path stops reporting the kill.
///
/// Cursor-based — it runs after every simulation step, so it only inspects
/// events it has not seen before.
struct DeadProcessIsSilentInvariant {
    fault_cursor: Cell<usize>,
    beat_cursor: Cell<usize>,
    /// Set once the crashed process is seen restarting.
    restarted: Cell<bool>,
    observations: SharedObservations,
}

impl DeadProcessIsSilentInvariant {
    fn new(observations: SharedObservations) -> Self {
        Self {
            fault_cursor: Cell::new(0),
            beat_cursor: Cell::new(0),
            restarted: Cell::new(false),
            observations,
        }
    }
}

impl Invariant for DeadProcessIsSilentInvariant {
    fn name(&self) -> &'static str {
        "dead_process_is_silent"
    }

    fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
        // Merge the two streams by global sequence number: a heartbeat and the
        // kill that silences it can land in the same batch.
        let mut fresh = q.since(SIM_FAULT_EVENT_NAME, &self.fault_cursor);
        fresh.extend(q.since(HEARTBEAT, &self.beat_cursor));
        if fresh.is_empty() {
            return;
        }
        fresh.sort_by_key(|event| event.seq);

        let mut observations = lock(&self.observations);

        for event in fresh {
            let Some((crashed_ip, crashed_at_ms)) = observations.crashed.clone() else {
                continue;
            };

            if event.name == HEARTBEAT {
                assert_always!(
                    self.restarted.get()
                        || event.source != crashed_ip
                        || event.time_ms <= crashed_at_ms,
                    format!(
                        "{} emitted a heartbeat at {}ms, after crashing at {crashed_at_ms}ms",
                        event.source, event.time_ms
                    )
                );
                continue;
            }

            let Some(kind @ ("process_force_kill" | "process_restart")) = event.str("kind") else {
                continue;
            };
            let ip = event.str("ip").expect("lifecycle fault carries an ip");
            observations
                .lifecycle
                .push(format!("{kind}@{ip}@{}", event.time_ms));
            if kind == "process_restart" && ip == crashed_ip && !self.restarted.replace(true) {
                observations.verified_windows += 1;
            }
        }
    }

    fn reset(&mut self) {
        self.fault_cursor.set(0);
        self.beat_cursor.set(0);
        self.restarted.set(false);
    }
}

/// Run one seed of the crash scenario, returning the report and what the
/// invariant observed.
fn run_crash_scenario(seed: u64) -> (moonpool_sim::SimulationReport, CrashObservations) {
    let observations: SharedObservations = std::sync::Arc::default();
    let report = SimulationBuilder::new()
        .processes(2, || Box::new(HeartbeatProcess))
        .workload(TimedWorkload(Duration::from_secs(8)))
        .fault(CrashOnceInjector {
            observations: observations.clone(),
        })
        .invariant(DeadProcessIsSilentInvariant::new(observations.clone()))
        .invariant(RebootTimingInvariant::new())
        .chaos_duration(Duration::from_secs(6))
        .set_iterations(1)
        .set_debug_seeds(vec![seed])
        .run();

    let observed = std::mem::take(&mut *lock(&observations));
    (report, observed)
}

#[test]
fn test_crash_reboot_silences_process_until_restart() {
    for seed in [42, 123, 999] {
        let (report, observed) = run_crash_scenario(seed);

        assert_eq!(
            report.failed_runs, 0,
            "seed {seed}: a crashed process must emit nothing until it restarts"
        );
        assert_eq!(
            observed.verified_windows, 1,
            "seed {seed}: expected exactly one crash → restart window, saw {:?}",
            observed.lifecycle
        );

        // The crash records a force-kill at crash time and a restart one
        // recovery delay later.
        let times: Vec<u64> = observed
            .lifecycle
            .iter()
            .map(|entry| {
                entry
                    .rsplit('@')
                    .next()
                    .and_then(|ms| ms.parse().ok())
                    .expect("lifecycle entry ends with a timestamp")
            })
            .collect();
        assert_eq!(
            observed.lifecycle.len(),
            2,
            "seed {seed}: expected force-kill then restart, saw {:?}",
            observed.lifecycle
        );
        assert!(
            observed.lifecycle[0].starts_with("process_force_kill@"),
            "seed {seed}: first lifecycle fault should be the force-kill, saw {:?}",
            observed.lifecycle
        );
        assert!(
            observed.lifecycle[1].starts_with("process_restart@"),
            "seed {seed}: second lifecycle fault should be the restart, saw {:?}",
            observed.lifecycle
        );
        assert_eq!(
            times[1] - times[0],
            CRASH_RECOVERY_MS as u64,
            "seed {seed}: restart should land one recovery delay after the kill"
        );
    }
}

#[test]
fn test_crash_reboot_replays_identically_for_a_seed() {
    let (first_report, first) = run_crash_scenario(7);
    let (second_report, second) = run_crash_scenario(7);

    assert_eq!(first_report.failed_runs, 0);
    assert_eq!(second_report.failed_runs, 0);
    assert_eq!(
        first.lifecycle, second.lifecycle,
        "the same seed must replay the same kill and restart times"
    );
    assert!(
        !first.lifecycle.is_empty(),
        "the scenario must actually crash a process"
    );
}
