//! The qualification script: during the chaos window it mixes every
//! lifecycle and trust transition the other campaigns exercised apart —
//! servers crashed and held down, rebooted in place and shut down
//! gracefully at their own address; the third participant and the legacy
//! peer restarted; the workload cut off from one server while it holds a
//! reliable call there, or from every server at once; the UTC stepped and
//! jumped past token lifetimes, lost and restored; the key set rotated.
//! The reboot-storm variant restarts one server at least ten times. At the
//! end it restores the clock and marks itself done; everything after that
//! is the workload's recovery phase.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    FaultContext, FaultInjector, RandomProvider, RebootKind, SimulationError, SimulationResult,
    TimeProvider, assert_reachable,
};

use super::state::{
    CUT_REQUESTS_KEY, QualLedger, SCRIPT_DONE_KEY, SHUTDOWN_REQUESTS_KEY, WORKLOAD_IP_KEY,
};
use crate::security::trust::Trust;

/// The campaign's fault script.
#[derive(Debug, Default)]
pub struct QualificationFaults {
    /// Restart one server in place over and over.
    pub storm: bool,
    cuts_handled: usize,
    shutdowns_handled: usize,
    storm_reboots: u64,
}

impl QualificationFaults {
    /// The reboot-storm variant.
    #[must_use]
    pub fn storm() -> Self {
        Self {
            storm: true,
            ..Self::default()
        }
    }
}

/// How long a requested cut lasts: past the ping timeout, so the session
/// fails and reliable calls are sent again on the next one.
const CUT: std::ops::Range<u64> = 3000..6000;

async fn pause(ctx: &FaultContext, millis: u64) -> SimulationResult<()> {
    ctx.time()
        .sleep(Duration::from_millis(millis))
        .await
        .map_err(|error| SimulationError::InvalidState(format!("sleep failed: {error}")))
}

impl QualificationFaults {
    /// Cut the workload off from each server it asked about (it holds a
    /// reliable call there), then heal.
    async fn serve_cuts(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let asked = ctx
            .state()
            .get::<Vec<(String, String)>>(CUT_REQUESTS_KEY)
            .unwrap_or_default();
        while self.cuts_handled < asked.len() {
            let (workload, server) = &asked[self.cuts_handled];
            self.cuts_handled += 1;
            ctx.partition(workload, server)?;
            assert_reachable!("rpc qual lane cut off from a server holding its reliable call");
            pause(ctx, ctx.random().random_range(CUT)).await?;
            ctx.heal_partition(workload, server)?;
        }
        Ok(())
    }

    /// Shut each server a lane asked about down gracefully (it holds calls
    /// and a stream there).
    fn serve_shutdowns(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let asked = ctx
            .state()
            .get::<Vec<String>>(SHUTDOWN_REQUESTS_KEY)
            .unwrap_or_default();
        while self.shutdowns_handled < asked.len() {
            let server = &asked[self.shutdowns_handled];
            self.shutdowns_handled += 1;
            QualLedger::of(ctx.state()).down(server);
            ctx.reboot_with_delays(server, RebootKind::Graceful, &(50..800), &(1500..2500))?;
            assert_reachable!("rpc qual server shut down gracefully on request");
        }
        Ok(())
    }

    async fn isolate_workload(
        &self,
        ctx: &FaultContext,
        servers: &[String],
    ) -> SimulationResult<()> {
        let lanes = ctx
            .state()
            .get::<Vec<String>>(WORKLOAD_IP_KEY)
            .unwrap_or_default();
        let Some(workload) = lanes.get(ctx.random().random_range(0..lanes.len().max(1))) else {
            return Ok(());
        };
        for server in servers {
            ctx.partition(workload, server)?;
        }
        assert_reachable!("rpc qual client partitioned from every server");
        pause(ctx, ctx.random().random_range(800..3000)).await?;
        for server in servers {
            ctx.heal_partition(workload, server)?;
        }
        Ok(())
    }

    async fn lifecycle(
        &mut self,
        ctx: &FaultContext,
        victim: &str,
        draw: u8,
    ) -> SimulationResult<()> {
        match draw {
            0 => {
                QualLedger::of(ctx.state()).down(victim);
                ctx.crash(victim)?;
                assert_reachable!("rpc qual server crashed and held down");
                pause(ctx, ctx.random().random_range(50..1500)).await?;
                ctx.restart(victim)?;
            }
            1 => {
                QualLedger::of(ctx.state()).down(victim);
                ctx.restart(victim)?;
                assert_reachable!("rpc qual server rebooted in place");
            }
            _ => {
                QualLedger::of(ctx.state()).down(victim);
                ctx.reboot_with_delays(victim, RebootKind::Graceful, &(50..800), &(300..1500))?;
                assert_reachable!("rpc qual server shut down gracefully under load");
            }
        }
        Ok(())
    }
}

#[async_trait]
impl FaultInjector for QualificationFaults {
    fn name(&self) -> &'static str {
        "rpc_qualification_script"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let trust = Trust::of(ctx.state())?;
        let servers = ctx.ips_in_group("server");
        let third = ctx.ips_in_group("third").into_iter().next();
        let legacy = ctx.ips_in_group("legacy").into_iter().next();
        pause(ctx, ctx.random().random_range(300..1200)).await?;
        while !ctx.chaos_shutdown().is_cancelled() {
            pause(ctx, ctx.random().random_range(150..600)).await?;
            self.serve_cuts(ctx).await?;
            self.serve_shutdowns(ctx)?;
            if self.storm && !servers.is_empty() {
                // One server, restarted in place again and again while the
                // workload keeps using it.
                ctx.restart(&servers[0])?;
                self.storm_reboots += 1;
                if self.storm_reboots >= 10 {
                    assert_reachable!("rpc qual one server rebooted ten times");
                }
                continue;
            }
            match ctx.random().random_range(0..18u8) {
                0..=3 => trust.advance(ctx.random().random_range(1..15)),
                4 => {
                    trust.advance(ctx.random().random_range(60..900));
                    assert_reachable!("rpc qual UTC stepped past token lifetimes");
                }
                5 | 6 => {
                    let generation = trust.rotate()?;
                    tracing::info!(line = %format!("keys rotated to {generation}"), "rpc_qual_event");
                    assert_reachable!("rpc qual key set rotated");
                }
                7 => {
                    trust.forget();
                    pause(ctx, ctx.random().random_range(100..600)).await?;
                    trust.restore();
                }
                8..=11 if !servers.is_empty() => {
                    let victim = servers[ctx.random().random_range(0..servers.len())].clone();
                    let draw = ctx.random().random_range(0..4u8);
                    self.lifecycle(ctx, &victim, draw).await?;
                }
                12 => {
                    if let Some(third) = &third {
                        if ctx.random().random_bool(0.5) {
                            ctx.crash(third)?;
                            pause(ctx, ctx.random().random_range(50..800)).await?;
                        }
                        ctx.restart(third)?;
                        assert_reachable!("rpc qual third participant restarted");
                    }
                }
                13 => {
                    if let Some(legacy) = &legacy {
                        ctx.restart(legacy)?;
                        assert_reachable!("rpc qual legacy peer restarted");
                    }
                }
                14 => self.isolate_workload(ctx, &servers).await?,
                _ => {}
            }
        }
        trust.restore();
        ctx.state().publish(SCRIPT_DONE_KEY, true);
        Ok(())
    }
}
