//! Operator example: a workload changes the deployment mid-run.
//!
//! A cell of [`RackNode`]s starts on a 1-datacenter × 2-zone × 2-machine
//! topology. While a client pings the nodes, an [`OperatorWorkload`] racks
//! new machines (`ctx.deployment().spawn` and `spawn_in`) and decommissions
//! old ones (`retire`), under process attrition. It is an ordinary workload:
//! moonpool has no operator actor, only the deployment handle every workload
//! gets.
//!
//! - [`RackNode`] checks its locality on every boot and echoes pings.
//! - [`OperatorWorkload`] keeps at least [`MIN_NODES`] nodes live, and checks
//!   that the topology follows each change.
//! - [`PingWorkload`] pings the nodes the topology lists now, so it reaches
//!   spawned nodes and stops reaching retired ones.

use std::time::Duration;

use async_trait::async_trait;
use futures::io::{AsyncReadExt, AsyncWriteExt};

use moonpool_sim::{
    DomainSpec, NetworkProvider, Process, RandomProvider, SimContext, SimulationResult,
    TcpListenerTrait, TimeProvider, Workload, assert_always, assert_sometimes,
};

use crate::support::{invalid_state, unless_shutdown};

/// The process group of the cell's nodes.
pub const GROUP: &str = "rack_node";

/// The operator never retires below this many live nodes.
pub const MIN_NODES: usize = 2;

/// A node that checks its locality on boot and echoes pings.
pub struct RackNode;

#[async_trait]
impl Process for RackNode {
    fn name(&self) -> &'static str {
        GROUP
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let locality = ctx.topology().my_locality().cloned();
        assert_always!(locality.is_some(), "rack node: every node has a locality");
        tracing::info!(
            rank = ctx.client_id(),
            machine = %locality.as_ref().map_or("", |l| l.machine()),
            "rack_node_booted"
        );
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        loop {
            let Some(accepted) = unless_shutdown(ctx, listener.accept()).await else {
                return Ok(());
            };
            if let Ok((mut stream, _)) = accepted {
                let mut buf = [0u8; 8];
                if let Ok(n) = stream.read(&mut buf).await
                    && n > 0
                {
                    let _ = stream.write_all(&buf[..n]).await;
                }
            }
        }
    }
}

/// Racks and decommissions nodes while the client runs.
pub struct OperatorWorkload;

#[async_trait]
impl Workload for OperatorWorkload {
    fn name(&self) -> &'static str {
        "operator"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        for _ in 0..12 {
            ctx.time()
                .sleep(Duration::from_millis(ctx.random().random_range(300..1500)))
                .await
                .map_err(|e| invalid_state(format!("sleep failed: {e}")))?;
            let live = ctx.topology().ips_in_group(GROUP);
            let version = ctx.deployment().version();
            if live.len() > MIN_NODES && ctx.random().random_bool(0.4) {
                let victim = live[ctx.random().random_range(0..live.len())].clone();
                ctx.deployment().retire(&victim)?;
                assert_always!(
                    !ctx.topology().ips_in_group(GROUP).contains(&victim),
                    "operator: a retired node leaves the topology at once"
                );
                assert_sometimes!(true, "operator: a node is decommissioned");
            } else {
                let spawned = if ctx.random().random_bool(0.5) {
                    ctx.deployment().spawn(GROUP)
                } else {
                    ctx.deployment().spawn_in(
                        GROUP,
                        DomainSpec::NewZoneIn {
                            datacenter: "dc1".to_string(),
                        },
                    )
                };
                // The run's spawn cap is the operator's budget.
                let Ok(ip) = spawned else {
                    continue;
                };
                assert_always!(
                    !live.contains(&ip),
                    "operator: a spawned node takes a fresh IP"
                );
                assert_sometimes!(true, "operator: a node is racked");
            }
            ctx.time()
                .sleep(Duration::from_millis(10))
                .await
                .map_err(|e| invalid_state(format!("sleep failed: {e}")))?;
            assert_always!(
                ctx.deployment().version() > version,
                "operator: every change moves the membership version"
            );
        }
        Ok(())
    }
}

/// Send one ping to `target` and read the echo.
async fn ping(ctx: &SimContext, target: &str) -> SimulationResult<()> {
    let mut stream = ctx.network().connect(target).await?;
    stream
        .write_all(b"ping")
        .await
        .map_err(|e| invalid_state(format!("write failed: {e}")))?;
    let mut buf = [0u8; 8];
    stream
        .read(&mut buf)
        .await
        .map_err(|e| invalid_state(format!("read failed: {e}")))?;
    Ok(())
}

/// Pings the nodes the topology lists now; a reboot or a retire in flight
/// breaks single pings, which is expected.
pub struct PingWorkload;

#[async_trait]
impl Workload for PingWorkload {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        for _ in 0..100 {
            if ctx.shutdown().is_cancelled() {
                break;
            }
            let nodes = ctx.topology().ips_in_group(GROUP);
            if !nodes.is_empty() {
                let target = nodes[ctx.random().random_range(0..nodes.len())].clone();
                let answered = ctx
                    .time()
                    .timeout(Duration::from_secs(2), ping(ctx, &target))
                    .await
                    .is_ok_and(|reply| reply.is_ok());
                let spawned = target
                    .parse()
                    .is_ok_and(|ip| ctx.topology().membership().was_spawned(ip));
                if answered && spawned {
                    assert_sometimes!(true, "operator: a racked node answers a ping");
                }
            }
            ctx.time()
                .sleep(Duration::from_millis(200))
                .await
                .map_err(|e| invalid_state(format!("sleep failed: {e}")))?;
        }
        Ok(())
    }
}
