//! The scripted fault: crash the server right after a handler received a
//! crash-after-receipt request, then restart it at the same address.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    FaultContext, FaultInjector, RandomProvider, SimulationError, SimulationResult, TimeProvider,
    assert_reachable,
};

use super::state::CRASH_REQUESTS_KEY;

/// Crashes the server once per crash-after-receipt request it sees.
#[derive(Debug, Default)]
pub struct CrashAfterReceiptInjector {
    handled: u64,
}

impl CrashAfterReceiptInjector {
    async fn pause(ctx: &FaultContext, duration: Duration) -> SimulationResult<()> {
        ctx.time()
            .sleep(duration)
            .await
            .map_err(|error| SimulationError::InvalidState(format!("sleep failed: {error}")))
    }
}

#[async_trait]
impl FaultInjector for CrashAfterReceiptInjector {
    fn name(&self) -> &'static str {
        "rpc_crash_after_receipt"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let Some(server) = ctx.ips_in_group("server").into_iter().next() else {
            return Ok(());
        };
        while !ctx.chaos_shutdown().is_cancelled() {
            Self::pause(ctx, Duration::from_millis(20)).await?;
            let requested = ctx.state().get::<u64>(CRASH_REQUESTS_KEY).unwrap_or(0);
            if requested <= self.handled {
                continue;
            }
            self.handled = requested;
            ctx.crash(&server)?;
            assert_reachable!("rpc server crashed after a handler received a request");
            // Down for a while, then back at the same ip:port with a fresh
            // incarnation. Always restarted, even past the chaos cutoff.
            let down = ctx.random().random_range(100..800);
            Self::pause(ctx, Duration::from_millis(down)).await?;
            ctx.restart(&server)?;
        }
        Ok(())
    }
}
