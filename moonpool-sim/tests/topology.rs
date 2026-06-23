//! Failure-domain topology / locality tests.
//!
//! Covers the `.cluster()` builder, the `WorkloadTopology` locality accessors,
//! `MachineRegistry` domain queries, and machine-scoped attrition (collocated
//! processes reboot together, respecting `max_dead`).

use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    Attrition, AttritionScope, Chaos, ChaosMode, DomainLevel, FaultContext, FaultInjector,
    LocalityConfig, NetworkProvider, Process, RebootKind, SimContext, SimulationBuilder,
    SimulationError, SimulationResult, TcpListenerTrait, TimeProvider, Workload,
};

// ============================================================================
// Shared process: validates its own locality on boot, then idles.
// ============================================================================

struct LocalityProcess {
    /// Expected number of collocated peers (`processes_per_machine - 1`).
    expected_collocated: usize,
}

#[async_trait]
impl Process for LocalityProcess {
    fn name(&self) -> &'static str {
        "locality_node"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let topo = ctx.topology();
        let locality = topo.my_locality().cloned().ok_or_else(|| {
            SimulationError::InvalidState("process booted without locality".into())
        })?;

        // Locality ids are hierarchical: machine starts with zone starts with dc.
        if !locality.machine().starts_with(locality.zone())
            || !locality.zone().starts_with(locality.datacenter())
        {
            return Err(SimulationError::InvalidState(format!(
                "non-hierarchical locality: {}/{}/{}",
                locality.datacenter(),
                locality.zone(),
                locality.machine()
            )));
        }

        let collocated = topo.peers_on_my_machine();
        if collocated.len() != self.expected_collocated {
            return Err(SimulationError::InvalidState(format!(
                "expected {} collocated peer(s), saw {}",
                self.expected_collocated,
                collocated.len()
            )));
        }

        // Bind so the workload can connect, then idle until shutdown.
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        loop {
            tokio::select! {
                _ = listener.accept() => {}
                () = ctx.shutdown().cancelled() => return Ok(()),
            }
        }
    }
}

/// Run for a fixed sim-time duration, then return so the iteration completes.
struct TimedWorkload(Duration);

#[async_trait]
impl Workload for TimedWorkload {
    fn name(&self) -> &'static str {
        "timed"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.time()
            .sleep(self.0)
            .await
            .map_err(|e| SimulationError::InvalidState(format!("sleep failed: {e}")))
    }
}

// ============================================================================
// Test: `.cluster()` assigns locality and domain queries are answerable.
// ============================================================================

/// Validates the registry-level domain queries that workloads can run.
struct DomainQueryWorkload;

#[async_trait]
impl Workload for DomainQueryWorkload {
    fn name(&self) -> &'static str {
        "domain_query"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let topo = ctx.topology();
        // 2 dc × 1 zone × 2 machine × 2 proc = 8 processes.
        let total = topo.all_process_ips().len();
        let dc1 = topo.ips_in_domain(DomainLevel::Datacenter, "dc1").len();
        let zone = topo.ips_in_domain(DomainLevel::Zone, "dc1-z1").len();
        let machine = topo.ips_in_domain(DomainLevel::Machine, "dc1-z1-m1").len();
        let first = topo
            .locality_for("10.0.1.1")
            .map(|l| l.machine().to_string());

        let ok = total == 8 && dc1 == 4 && zone == 4 && machine == 2;
        if !ok || first.as_deref() != Some("dc1-z1-m1") {
            return Err(SimulationError::InvalidState(format!(
                "bad domain queries: total={total} dc1={dc1} zone={zone} machine={machine} first={first:?}"
            )));
        }
        Ok(())
    }
}

#[test]
fn cluster_assigns_locality_and_answers_domain_queries() {
    let report = SimulationBuilder::new()
        .cluster(LocalityConfig::new(2, 1, 2, 2), || {
            Box::new(LocalityProcess {
                expected_collocated: 1,
            })
        })
        .workload(DomainQueryWorkload)
        .set_iterations(1)
        .set_debug_seeds(vec![42])
        .run();

    assert_eq!(report.successful_runs, 1, "domain queries should hold");
    assert_eq!(report.failed_runs, 0);
}

// ============================================================================
// Test: a machine reboot kills both collocated processes together.
// ============================================================================

/// Reboots one machine via [`FaultContext::reboot_machine`] and asserts that
/// both collocated processes were rebooted as a group.
struct MachineRebootInjector;

#[async_trait]
impl FaultInjector for MachineRebootInjector {
    fn name(&self) -> &'static str {
        "machine_reboot"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        ctx.time()
            .sleep(Duration::from_millis(100))
            .await
            .map_err(|e| SimulationError::InvalidState(format!("sleep failed: {e}")))?;

        let rebooted = ctx.reboot_machine("dc1-z1-m1", RebootKind::Crash)?;
        // The machine hosts exactly 2 processes; both must reboot together.
        if rebooted.len() != 2 {
            return Err(SimulationError::InvalidState(format!(
                "machine reboot hit {} processes, expected 2 (collocated)",
                rebooted.len()
            )));
        }
        let expected = ["10.0.1.1".to_string(), "10.0.1.2".to_string()];
        if !rebooted.iter().all(|ip| expected.contains(ip)) {
            return Err(SimulationError::InvalidState(format!(
                "machine reboot hit unexpected IPs: {rebooted:?}"
            )));
        }

        ctx.chaos_shutdown().cancelled().await;
        Ok(())
    }
}

#[test]
fn machine_reboot_is_collocated() {
    let report = SimulationBuilder::new()
        .cluster(LocalityConfig::new(1, 1, 2, 2), || {
            Box::new(LocalityProcess {
                expected_collocated: 1,
            })
        })
        .workload(TimedWorkload(Duration::from_secs(20)))
        .fault(MachineRebootInjector)
        .chaos_duration(Duration::from_secs(5))
        .set_iterations(1)
        .set_debug_seeds(vec![42])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "collocated machine reboot + recovery should succeed"
    );
}

// ============================================================================
// Test: built-in machine-scoped attrition respects the max_dead group budget.
// ============================================================================

#[test]
fn per_machine_attrition_respects_group_budget() {
    let report = SimulationBuilder::new()
        // 1 dc × 1 zone × 3 machines × 2 procs = 6 processes.
        .cluster(LocalityConfig::new(1, 1, 3, 2), || {
            Box::new(LocalityProcess {
                expected_collocated: 1,
            })
        })
        .workload(TimedWorkload(Duration::from_secs(25)))
        .enable_chaos([Chaos::Attrition {
            config: Attrition {
                // Exactly one machine's worth of processes may be dead at once.
                max_dead: 2,
                prob_graceful: 0.3,
                prob_crash: 0.5,
                prob_wipe: 0.2,
                recovery_delay_ms: None,
                grace_period_ms: None,
                scope: AttritionScope::PerMachine,
            },
            mode: ChaosMode::Random,
        }])
        .chaos_duration(Duration::from_secs(10))
        .set_iterations(5)
        .set_debug_seeds(vec![1, 2, 3, 4, 5])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "machine-scoped attrition should not deadlock or fail"
    );
}

// ============================================================================
// Test: per-seed range topology (randomized cluster shape) runs clean.
// ============================================================================

/// Validates that whatever topology a seed samples, every process has locality
/// and its datacenter holds at least one machine's worth of processes.
struct RangeTopologyWorkload;

#[async_trait]
impl Workload for RangeTopologyWorkload {
    fn name(&self) -> &'static str {
        "range_topology"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let topo = ctx.topology();
        // Every process IP resolves to a locality.
        for ip in topo.all_process_ips() {
            if topo.locality_for(ip).is_none() {
                return Err(SimulationError::InvalidState(format!(
                    "process {ip} has no locality"
                )));
            }
        }
        Ok(())
    }
}

#[test]
fn range_topology_runs_clean_across_seeds() {
    let report = SimulationBuilder::new()
        // Datacenter and machine counts are randomized per seed.
        .cluster(LocalityConfig::new(1..=3, 1, 1..=3, 2), || {
            Box::new(LocalityProcess {
                expected_collocated: 1,
            })
        })
        .workload(RangeTopologyWorkload)
        .set_iterations(8)
        .set_debug_seeds(vec![10, 20, 30, 40, 50, 60, 70, 80])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "range topologies should all be valid"
    );
    assert_eq!(report.successful_runs, 8);
}
