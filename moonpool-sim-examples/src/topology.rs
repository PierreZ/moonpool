//! Failure-domain topology example.
//!
//! Models a 3-datacenter × 3-zone × 3-machine cluster with two processes per
//! machine (54 processes total) under **machine-scoped attrition**, so the two
//! collocated processes on a machine share fate and reboot together.
//!
//! - [`TopologyProcess`] validates its assigned locality on every boot
//!   (`my_locality`, `peers_on_my_machine`, `ips_in_domain`) and serves a tiny
//!   echo protocol.
//! - [`TopologyWorkload`] pings random nodes; pings broken by a machine reboot
//!   are expected and tolerated. The run must complete without deadlock.

use std::time::Duration;

use async_trait::async_trait;
use futures::io::{AsyncReadExt, AsyncWriteExt};

use moonpool_sim::{
    DomainLevel, NetworkProvider, Process, RandomProvider, SimContext, SimulationError,
    SimulationResult, TcpListenerTrait, TimeProvider, Workload, assert_always,
};

/// Processes collocated on each machine — the shared-fate group size.
pub const PROCESSES_PER_MACHINE: usize = 2;

/// Zones per datacenter × machines per zone × processes per machine.
const PROCESSES_PER_DATACENTER: usize = 3 * 3 * PROCESSES_PER_MACHINE;

/// A health-check node that validates its failure-domain locality on boot.
pub struct TopologyProcess;

#[async_trait]
impl Process for TopologyProcess {
    fn name(&self) -> &'static str {
        "topology_node"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let topo = ctx.topology();
        let locality = topo.my_locality().cloned().ok_or_else(|| {
            SimulationError::InvalidState("process booted without locality".into())
        })?;
        tracing::info!(
            dc = %locality.datacenter(),
            zone = %locality.zone(),
            machine = %locality.machine(),
            "node_booted"
        );

        // Shared fate: this process sees its machine-mates as collocated peers.
        let collocated = topo.peers_on_my_machine();
        assert_always!(
            collocated.len() == PROCESSES_PER_MACHINE - 1,
            format!(
                "expected {} collocated peer(s), saw {}",
                PROCESSES_PER_MACHINE - 1,
                collocated.len()
            )
        );

        // Domain query: every datacenter holds the same number of processes.
        let in_dc = topo.ips_in_domain(DomainLevel::Datacenter, locality.datacenter());
        assert_always!(
            in_dc.len() == PROCESSES_PER_DATACENTER,
            format!(
                "datacenter {} should hold {} processes, saw {}",
                locality.datacenter(),
                PROCESSES_PER_DATACENTER,
                in_dc.len()
            )
        );

        let listener = ctx.network().bind(ctx.my_ip()).await?;
        loop {
            let accepted = tokio::select! {
                r = listener.accept() => r,
                () = ctx.shutdown().cancelled() => return Ok(()),
            };
            if let Ok((mut stream, _)) = accepted {
                let mut buf = [0u8; 32];
                if let Ok(n) = stream.read(&mut buf).await
                    && n > 0
                {
                    let _ = stream.write_all(&buf[..n]).await;
                }
            }
        }
    }
}

/// Send a single ping to `target` and read the echo. Errors (a reboot in
/// flight) are intentionally swallowed by the caller.
async fn ping(ctx: &SimContext, target: &str) -> SimulationResult<()> {
    let mut stream = ctx.network().connect(target).await?;
    stream
        .write_all(b"ping")
        .await
        .map_err(|e| SimulationError::InvalidState(format!("write failed: {e}")))?;
    let mut buf = [0u8; 8];
    stream
        .read(&mut buf)
        .await
        .map_err(|e| SimulationError::InvalidState(format!("read failed: {e}")))?;
    Ok(())
}

/// A client that pings random nodes; reboots break individual pings, which is
/// expected. The workload only needs the simulation to make progress.
pub struct TopologyWorkload;

#[async_trait]
impl Workload for TopologyWorkload {
    fn name(&self) -> &'static str {
        "topology_client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let process_ips: Vec<String> = ctx.topology().all_process_ips().to_vec();
        assert_always!(
            !process_ips.is_empty(),
            "workload should see server processes"
        );

        // Drive ~16s of sim-time traffic (outlasting the 10s chaos window),
        // then return Ok so the iteration completes. Returning Ok triggers the
        // shutdown signal; a workload that loops forever would deadlock the run.
        for _round in 0..80 {
            if ctx.shutdown().is_cancelled() {
                break;
            }
            let idx = ctx.random().random_range(0..process_ips.len());
            let target = process_ips[idx].clone();
            // Connecting to a node mid-reboot can hang; bound each ping with a
            // timeout so the workload always makes progress. A reboot in flight
            // surfaces as a connect/IO error or a timeout — both are expected.
            let _ = ctx
                .time()
                .timeout(Duration::from_secs(2), ping(ctx, &target))
                .await;
            ctx.time()
                .sleep(Duration::from_millis(200))
                .await
                .map_err(|e| SimulationError::InvalidState(format!("sleep failed: {e}")))?;
        }
        Ok(())
    }
}
