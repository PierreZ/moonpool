//! The scripted faults: the workload asks for a producer reboot while one
//! of its streams is running, and the script crashes the producer (held
//! down, then restarted at the same address) or shuts it down gracefully.
//! Network chaos runs alongside, drawn per seed.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    FaultContext, FaultInjector, RandomProvider, RebootKind, SimulationError, SimulationResult,
    TimeProvider, assert_reachable,
};

use super::state::{FAULTS_DONE_KEY, REBOOT_REQUESTS_KEY};

/// Serves the workload's reboot requests during the chaos window.
#[derive(Debug, Default)]
pub struct StreamsFaults {
    handled: usize,
}

async fn pause(ctx: &FaultContext, millis: u64) -> SimulationResult<()> {
    ctx.time()
        .sleep(Duration::from_millis(millis))
        .await
        .map_err(|error| SimulationError::InvalidState(format!("sleep failed: {error}")))
}

impl StreamsFaults {
    async fn serve(&mut self, ctx: &FaultContext, producer: &str) -> SimulationResult<()> {
        let requests = ctx
            .state()
            .get::<Vec<bool>>(REBOOT_REQUESTS_KEY)
            .unwrap_or_default();
        while self.handled < requests.len() {
            let graceful = requests[self.handled];
            self.handled += 1;
            if graceful {
                ctx.reboot_with_delays(producer, RebootKind::Graceful, &(50..400), &(20..200))?;
                assert_reachable!("rpc stream producer shut down gracefully mid-stream");
            } else {
                ctx.crash(producer)?;
                assert_reachable!("rpc stream producer crashed mid-stream");
                pause(ctx, ctx.random().random_range(100..1500)).await?;
                ctx.restart(producer)?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl FaultInjector for StreamsFaults {
    fn name(&self) -> &'static str {
        "rpc_streams_faults"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let Some(producer) = ctx.ips_in_group("producer").into_iter().next() else {
            return Ok(());
        };
        while !ctx.chaos_shutdown().is_cancelled() {
            pause(ctx, 20).await?;
            self.serve(ctx, &producer).await?;
        }
        // Tell the workload chaos is over, then serve what raced the flag.
        ctx.state().publish(FAULTS_DONE_KEY, true);
        pause(ctx, 50).await?;
        self.serve(ctx, &producer).await
    }
}
