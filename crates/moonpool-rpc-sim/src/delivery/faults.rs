//! The scripted faults: DNS changes, lost replies and crashes, each fired
//! by a bounded deterministic script so the scenario is guaranteed to
//! happen on every seed that asks for it.

use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_rpc::RpcHandle;
use moonpool_sim::{
    FaultContext, FaultInjector, RandomProvider, ScriptedResolver, SimProviders, SimulationError,
    SimulationResult, TimeProvider, assert_reachable,
};

use super::RPC_PORT;
use super::state::{
    CRASH_REQUESTS_KEY, CUT_REQUESTS_KEY, CUTS_INSTALLED_KEY, RESOLVER_KEY, SCRIPT_DONE_KEY,
    SERVER_NAME, WORKLOAD_IP_KEY, WORKLOAD_RPC_KEY,
};

/// An address nothing listens on: the bootstrap name points here first.
const NOWHERE: [u8; 4] = [10, 0, 9, 9];

/// Lost replies are healed at the latest after this long.
const LONGEST_CUT: Duration = Duration::from_secs(20);

/// Drives every scripted fault of the campaign.
#[derive(Debug, Default)]
pub struct DeliveryFaults {
    cuts_handled: u64,
    crashes_handled: u64,
}

async fn pause(ctx: &FaultContext, duration: Duration) -> SimulationResult<()> {
    ctx.time()
        .sleep(duration)
        .await
        .map_err(|error| SimulationError::InvalidState(format!("sleep failed: {error}")))
}

impl DeliveryFaults {
    /// The bootstrap name: missing, then wrong, then right.
    async fn script_dns(ctx: &FaultContext, server: &str) -> SimulationResult<()> {
        let resolver = loop {
            if let Some(resolver) = ctx.state().get::<ScriptedResolver>(RESOLVER_KEY) {
                break resolver;
            }
            pause(ctx, Duration::from_millis(10)).await?;
        };
        pause(
            ctx,
            Duration::from_millis(ctx.random().random_range(50..300)),
        )
        .await?;
        resolver.set(SERVER_NAME, vec![IpAddr::from(NOWHERE)]);
        assert_reachable!("rpc bootstrap name pointed at a dead address");
        pause(
            ctx,
            Duration::from_millis(ctx.random().random_range(100..1500)),
        )
        .await?;
        let server: IpAddr = server
            .parse()
            .map_err(|error| SimulationError::InvalidState(format!("server ip: {error}")))?;
        resolver.set(SERVER_NAME, vec![server]);
        assert_reachable!("rpc bootstrap name repointed at the live server");
        Ok(())
    }

    /// Cut the workload off from the server until its runtime reports a
    /// disconnect of the server's address (its ping timeout, usually), then
    /// heal.
    async fn lose_reply(ctx: &FaultContext, server: &str, request: u64) -> SimulationResult<()> {
        let (Some(workload), Some(rpc)) = (
            ctx.state().get::<String>(WORKLOAD_IP_KEY),
            ctx.state().get::<RpcHandle<SimProviders>>(WORKLOAD_RPC_KEY),
        ) else {
            return Ok(());
        };
        let address = format!("{server}:{RPC_PORT}")
            .parse()
            .map_err(|error| SimulationError::InvalidState(format!("server address: {error}")))?;
        let disconnects = || {
            rpc.failure_monitor()
                .map_or(u64::MAX, |monitor| monitor.disconnects(address))
        };
        let before = disconnects();
        ctx.partition(&workload, server)?;
        ctx.state().publish(CUTS_INSTALLED_KEY, request);
        assert_reachable!("rpc workload cut off from the server after execution");
        let started = ctx.time().now();
        while disconnects() == before && ctx.time().now().saturating_sub(started) < LONGEST_CUT {
            // The simulator heals every partition it holds when chaos ends;
            // this scripted cut lasts until the workload noticed it.
            if !ctx.is_partitioned(&workload, server)? {
                ctx.partition(&workload, server)?;
            }
            pause(ctx, Duration::from_millis(20)).await?;
        }
        ctx.heal_partition(&workload, server)
    }

    /// Serve every crash or cut request received so far.
    async fn serve_requests(&mut self, ctx: &FaultContext, server: &str) -> SimulationResult<()> {
        let cuts = ctx.state().get::<u64>(CUT_REQUESTS_KEY).unwrap_or(0);
        if cuts > self.cuts_handled {
            self.cuts_handled = cuts;
            Self::lose_reply(ctx, server, cuts).await?;
        }
        let crashes = ctx.state().get::<u64>(CRASH_REQUESTS_KEY).unwrap_or(0);
        if crashes > self.crashes_handled {
            self.crashes_handled = crashes;
            Self::crash(ctx, server).await?;
        }
        Ok(())
    }

    async fn crash(ctx: &FaultContext, server: &str) -> SimulationResult<()> {
        ctx.crash(server)?;
        assert_reachable!("rpc delivery server crashed after a handler received a job");
        let down = ctx.random().random_range(100..2500);
        pause(ctx, Duration::from_millis(down)).await?;
        ctx.restart(server)
    }
}

impl DeliveryFaults {
    /// One-way reachability between the peers: cut them apart so their
    /// shared connection dies, and keep the larger address from dialing
    /// out while the smaller one can dial it (a firewall, not a network
    /// fault: the gate lives in the peers' session upgrade).
    async fn one_way(ctx: &FaultContext) -> SimulationResult<()> {
        let mut peers = ctx.ips_in_group("peer");
        peers.sort_by_key(|ip| ip.parse::<IpAddr>().ok());
        let (Some(smaller), Some(larger)) = (peers.first().cloned(), peers.last().cloned()) else {
            return Ok(());
        };
        if smaller == larger {
            return Ok(());
        }
        pause(
            ctx,
            Duration::from_millis(ctx.random().random_range(500..3000)),
        )
        .await?;
        let key = super::peer::no_dial_key(&larger);
        ctx.state().publish(&key, true);
        ctx.partition(&smaller, &larger)?;
        assert_reachable!("rpc peers cut apart with one-way reachability");
        pause(
            ctx,
            Duration::from_millis(ctx.random().random_range(1000..4000)),
        )
        .await?;
        ctx.heal_partition(&smaller, &larger)?;
        pause(
            ctx,
            Duration::from_millis(ctx.random().random_range(2000..6000)),
        )
        .await?;
        ctx.state().publish(&key, false);
        Ok(())
    }
}

#[async_trait]
impl FaultInjector for DeliveryFaults {
    fn name(&self) -> &'static str {
        "rpc_delivery_faults"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let Some(server) = ctx.ips_in_group("server").into_iter().next() else {
            return Ok(());
        };
        Self::script_dns(ctx, &server).await?;
        let mut one_way = Box::pin(Self::one_way(ctx));
        let mut one_way_done = false;
        while !ctx.chaos_shutdown().is_cancelled() {
            if !one_way_done
                && let std::task::Poll::Ready(result) = futures::poll!(one_way.as_mut())
            {
                result?;
                one_way_done = true;
            }
            pause(ctx, Duration::from_millis(20)).await?;
            self.serve_requests(ctx, &server).await?;
        }
        // Finish the one-way window (it always lifts the gate).
        if !one_way_done {
            one_way.await?;
        }
        // Tell the workload to stop asking, then serve what raced the flag.
        ctx.state().publish(SCRIPT_DONE_KEY, true);
        pause(ctx, Duration::from_millis(50)).await?;
        self.serve_requests(ctx, &server).await
    }
}
