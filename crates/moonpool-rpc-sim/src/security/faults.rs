//! The trust and lifecycle script: during the chaos window it moves the
//! scripted UTC forward (small steps and large jumps), rotates the key set,
//! makes the servers lose the time for a while, and crashes or gracefully
//! reboots the verifying server, all mixed with the network chaos. At the
//! end it restores the clock and marks itself done.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    FaultContext, FaultInjector, RandomProvider, RebootKind, SimulationError, SimulationResult,
    TimeProvider, assert_reachable,
};

use super::state::{CUT_REQUESTS_KEY, SCRIPT_DONE_KEY, WORKLOAD_IP_KEY};
use super::trust::Trust;

/// The campaign's fault script.
#[derive(Debug, Default)]
pub struct SecurityFaults {
    cuts_handled: u64,
}

/// How long a scripted cut lasts: past the ping timeout, so the session
/// fails and reliable calls are sent again on the next one.
const CUT: std::ops::Range<u64> = 3000..4500;

async fn pause(ctx: &FaultContext, millis: u64) -> SimulationResult<()> {
    ctx.time()
        .sleep(Duration::from_millis(millis))
        .await
        .map_err(|error| SimulationError::InvalidState(format!("sleep failed: {error}")))
}

impl SecurityFaults {
    /// Cut the workload off from the server for a while, when it asked
    /// (it holds a reliable call there).
    async fn cut_if_asked(&mut self, ctx: &FaultContext, server: &str) -> SimulationResult<()> {
        let asked = ctx.state().get::<u64>(CUT_REQUESTS_KEY).unwrap_or(0);
        let Some(workload) = ctx.state().get::<String>(WORKLOAD_IP_KEY) else {
            return Ok(());
        };
        if asked <= self.cuts_handled {
            return Ok(());
        }
        self.cuts_handled = asked;
        ctx.partition(&workload, server)?;
        assert_reachable!("rpc security workload cut off from the server");
        pause(ctx, ctx.random().random_range(CUT)).await?;
        ctx.heal_partition(&workload, server)
    }
}

#[async_trait]
impl FaultInjector for SecurityFaults {
    fn name(&self) -> &'static str {
        "rpc_security_script"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let trust = Trust::of(ctx.state())?;
        let server = ctx.ips_in_group("server").into_iter().next();
        while !ctx.chaos_shutdown().is_cancelled() {
            pause(ctx, ctx.random().random_range(100..500)).await?;
            if let Some(server) = &server {
                self.cut_if_asked(ctx, server).await?;
            }
            match ctx.random().random_range(0..13u8) {
                0..=4 => trust.advance(ctx.random().random_range(1..15)),
                5 => {
                    trust.advance(ctx.random().random_range(60..900));
                    assert_reachable!("rpc security UTC jumped forward");
                }
                6 | 7 => {
                    let generation = trust.rotate()?;
                    tracing::info!(generation, "rpc_security_keys_rotated");
                    assert_reachable!("rpc security key set rotated");
                }
                8 => {
                    trust.forget();
                    assert_reachable!("rpc security servers lost the UTC time");
                    pause(ctx, ctx.random().random_range(100..600)).await?;
                    trust.restore();
                }
                9 => {
                    if let Some(server) = &server {
                        ctx.crash(server)?;
                        assert_reachable!("rpc security server crashed mid-work");
                        pause(ctx, ctx.random().random_range(50..800)).await?;
                        ctx.restart(server)?;
                    }
                }
                _ => {
                    if let Some(server) = &server {
                        ctx.reboot_with_delays(
                            server,
                            RebootKind::Graceful,
                            &(50..600),
                            &(300..1200),
                        )?;
                        assert_reachable!("rpc security server shut down gracefully mid-work");
                    }
                }
            }
        }
        trust.restore();
        ctx.state().publish(SCRIPT_DONE_KEY, true);
        Ok(())
    }
}
