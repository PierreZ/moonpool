//! The fault script: during the chaos window, take every alternative away
//! (a partition between the workload and every server, which keeps their
//! incarnations, or a crash of every server, which does not) and bring
//! them back, and crash single servers in between.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    FaultContext, FaultInjector, RandomProvider, SimulationError, SimulationResult, TimeProvider,
    assert_reachable, buggify,
};

use super::state::{Ledger, SCRIPT_DONE_KEY, WORKLOAD_IP_KEY};

/// Takes alternatives away and restores them.
#[derive(Debug, Default)]
pub struct BalanceFaults;

async fn pause(ctx: &FaultContext, millis: std::ops::Range<u64>) -> SimulationResult<()> {
    let millis = ctx.random().random_range(millis);
    ctx.time()
        .sleep(Duration::from_millis(millis))
        .await
        .map_err(|error| SimulationError::InvalidState(format!("sleep failed: {error}")))
}

/// How long a whole outage holds: `usual`, or, when buggified, long enough
/// to outlast the client's failure detection, its `all_failed_wait` and its
/// probe, so a call reports that every alternative failed.
fn outage_millis(usual: std::ops::Range<u64>) -> std::ops::Range<u64> {
    if buggify!() { 2500..5000 } else { usual }
}

#[async_trait]
impl FaultInjector for BalanceFaults {
    fn name(&self) -> &'static str {
        "rpc_balance_outages"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let servers = ctx.ips_in_group("balance-server");
        let ledger = Ledger::of(ctx.state());
        if servers.is_empty() {
            ctx.state().publish(SCRIPT_DONE_KEY, true);
            return Ok(());
        }
        pause(ctx, 800..2000).await?;
        while !ctx.chaos_shutdown().is_cancelled() {
            match ctx.random().random_range(0..3u8) {
                0 => {
                    if let Some(workload) = ctx.state().get::<String>(WORKLOAD_IP_KEY) {
                        for server in &servers {
                            ctx.partition(&workload, server)?;
                        }
                        ledger.set_outage(true);
                        assert_reachable!("rpc balance every alternative partitioned away");
                        pause(ctx, outage_millis(400..2000)).await?;
                        for server in &servers {
                            ctx.heal_partition(&workload, server)?;
                        }
                        ledger.set_outage(false);
                    }
                }
                1 => {
                    for server in &servers {
                        ctx.crash(server)?;
                    }
                    ledger.set_outage(true);
                    assert_reachable!("rpc balance every server crashed");
                    pause(ctx, outage_millis(200..1500)).await?;
                    for server in &servers {
                        ctx.restart(server)?;
                    }
                    ledger.set_outage(false);
                }
                _ => {
                    let victim = &servers[ctx.random().random_range(0..servers.len())];
                    ctx.crash(victim)?;
                    assert_reachable!("rpc balance one server crashed");
                    pause(ctx, 100..1200).await?;
                    ctx.restart(victim)?;
                }
            }
            pause(ctx, 800..2500).await?;
        }
        ctx.state().publish(SCRIPT_DONE_KEY, true);
        Ok(())
    }
}
