//! The reboot script: during the chaos window, participants are restarted
//! at the same address again and again — crashed and held down, rebooted
//! in place with no downtime, or shut down gracefully — so every seed
//! crosses several same-address restarts.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    FaultContext, FaultInjector, RandomProvider, RebootKind, SimulationError, SimulationResult,
    TimeProvider, assert_reachable,
};

use super::state::SCRIPT_DONE_KEY;

/// Restarts participants at their own address.
#[derive(Debug, Default)]
pub struct InterfaceFaults;

async fn pause(ctx: &FaultContext, millis: u64) -> SimulationResult<()> {
    ctx.time()
        .sleep(Duration::from_millis(millis))
        .await
        .map_err(|error| SimulationError::InvalidState(format!("sleep failed: {error}")))
}

#[async_trait]
impl FaultInjector for InterfaceFaults {
    fn name(&self) -> &'static str {
        "rpc_interfaces_reboots"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let participants = ctx.ips_in_group("participant");
        if participants.is_empty() {
            return Ok(());
        }
        pause(ctx, ctx.random().random_range(300..1200)).await?;
        while !ctx.chaos_shutdown().is_cancelled() {
            let victim = &participants[ctx.random().random_range(0..participants.len())];
            match ctx.random().random_range(0..3u8) {
                0 => {
                    // Crash, stay down a while, come back at the same address.
                    ctx.crash(victim)?;
                    assert_reachable!("rpc participant crashed and held down");
                    pause(ctx, ctx.random().random_range(50..1500)).await?;
                    ctx.restart(victim)?;
                }
                1 => {
                    // Abort and boot a fresh instance at once.
                    ctx.restart(victim)?;
                    assert_reachable!("rpc participant rebooted in place");
                }
                _ => {
                    ctx.reboot_with_delays(victim, RebootKind::Graceful, &(50..800), &(20..400))?;
                    assert_reachable!("rpc participant shut down gracefully");
                }
            }
            pause(ctx, ctx.random().random_range(600..2500)).await?;
        }
        ctx.state().publish(SCRIPT_DONE_KEY, true);
        Ok(())
    }
}
