//! Two listening peers that call each other on the same schedule, so both
//! dial at once: simultaneous-connect resolution, shared sessions and
//! reconnects between two servers.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_rpc::{AccessClass, IncomingRequest, RequestStream, RpcDriver, RpcHandle, ServiceRef};
use moonpool_sim::{
    Process, RandomProvider, SimContext, SimProviders, SimulationError, SimulationResult,
    TimeProvider, assert_always, assert_sometimes,
};

use super::messages::{Greet, Greeting};
use super::policy::delivery_config;
use super::{RPC_PORT, report_stats};
use crate::foundations::state::{Board, bump};

/// State key of a peer's greeting reference, by its IP.
fn peer_key(ip: &str) -> String {
    format!("rpc.delivery.peer.{ip}")
}

/// One of the two peers.
pub struct DeliveryPeer;

#[async_trait]
impl Process for DeliveryPeer {
    fn name(&self) -> &'static str {
        "peer"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let state = ctx.state().clone();
        let me = ctx.my_ip().to_string();
        let boot = bump(&state, &format!("rpc.delivery.peer.boots.{me}"));
        let board = Board::of(&state);
        let label = format!("peer#{me}#{boot}");
        let address = format!("{me}:{RPC_PORT}");
        let (driver, rpc) = RpcDriver::listen(ctx.providers().clone(), &address, delivery_config())
            .await
            .map_err(|error| SimulationError::IoError(format!("rpc listen: {error}")))?;
        let (greet_ref, greet) = rpc
            .register::<Greet>(AccessClass::Public)
            .map_err(|error| SimulationError::InvalidState(format!("register: {error}")))?;
        state.publish(&peer_key(&me), greet_ref.to_bytes());
        let other = ctx
            .topology()
            .ips_in_group("peer")
            .into_iter()
            .find(|ip| *ip != me);
        let work = async {
            futures::join!(
                serve(greet),
                call_other(&rpc, ctx, other.as_deref()),
                report_stats(&rpc, &board, &label, ctx),
            );
        };
        moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            () = work => Ok(()),
            () = ctx.shutdown().cancelled() => Ok(()),
        }
    }
}

async fn serve(mut stream: RequestStream<Greet>) {
    while let Some(IncomingRequest { request, reply }) = stream.recv().await {
        let _ = reply.send(&request);
    }
}

/// Call the other peer at the same instants it calls us (every multiple of
/// the period), alternating single attempts and bounded reliable calls.
async fn call_other(rpc: &RpcHandle<SimProviders>, ctx: &SimContext, other: Option<&str>) {
    let Some(other) = other else {
        return;
    };
    let period = Duration::from_millis(600);
    let mut seq = 0u64;
    loop {
        let now = ctx.time().now();
        let into = Duration::from_nanos(
            u64::try_from(now.as_nanos() % period.as_nanos()).unwrap_or_default(),
        );
        if ctx.time().sleep(period.saturating_sub(into)).await.is_err() {
            return;
        }
        let Some(target) = ctx
            .state()
            .get::<Vec<u8>>(&peer_key(other))
            .and_then(|bytes| ServiceRef::<Greet>::from_bytes(&bytes).ok())
        else {
            continue;
        };
        seq += 1;
        let client = target.bind(rpc);
        let outcome = if ctx.random().random_bool(0.5) {
            client
                .try_get_reply_within(&Greeting { seq }, Duration::from_secs(2))
                .await
        } else {
            client
                .get_reply_unless_failed_for(&Greeting { seq }, Duration::from_secs(1), 0.1)
                .await
        };
        if let Ok(reply) = &outcome {
            assert_always!(reply.seq == seq, "a peer's reply matches its greeting");
        }
        if let Some(stats) = rpc.stats() {
            assert_sometimes!(
                stats.redundant_connections > 0 && outcome.is_ok(),
                "rpc simultaneous connect resolved to one shared connection"
            );
            assert_sometimes!(
                stats.adopted_connections > 0,
                "rpc accepted session adopted as the peer connection"
            );
        }
    }
}
