//! Two listening peers that call each other on the same schedule, so both
//! dial at once: simultaneous-connect resolution, shared sessions and
//! reconnects between two servers.

use std::io;
use std::time::Duration;

use async_trait::async_trait;
use futures::io::{AsyncRead, AsyncWrite};
use moonpool_rpc::{
    Acceptor, AccessClass, Connector, InboundSharing, IncomingRequest, PeerContext, RequestStream,
    RpcDriver, RpcHandle, ServiceRef,
};
use moonpool_sim::{
    Process, RandomProvider, SimContext, SimProviders, SimulationError, SimulationResult,
    StateHandle, TimeProvider, assert_always, assert_reachable, assert_sometimes,
};

use super::messages::{Greet, Greeting};
use super::policy::{delivery_config_sharing, sharing_from};
use super::{RPC_PORT, report_stats};
use crate::foundations::state::{Board, bump};

/// State key of a peer's greeting reference, by its IP.
fn peer_key(ip: &str) -> String {
    format!("rpc.delivery.peer.{ip}")
}

/// State key of a peer's current sharing mode, by its IP.
pub(crate) fn sharing_key(ip: &str) -> String {
    format!("rpc.delivery.peer.sharing.{ip}")
}

/// State key that, while `true`, makes every dial of the peer at `ip`
/// fail: one-way reachability (others can still dial it).
pub(crate) fn no_dial_key(ip: &str) -> String {
    format!("rpc.delivery.peer.nodial.{ip}")
}

/// A session upgrade that refuses this process's outbound dials while the
/// fault script says so, and passes everything else through. It models a
/// firewall that lets a peer be dialed but not dial out.
#[derive(Clone)]
pub struct DialGate {
    state: StateHandle,
    key: String,
}

impl DialGate {
    /// The gate of the process at `ip`.
    #[must_use]
    pub fn new(state: StateHandle, ip: &str) -> Self {
        Self {
            state,
            key: no_dial_key(ip),
        }
    }
}

impl<S> Connector<S> for DialGate
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = S;

    async fn connect(&self, stream: S, peer: &str) -> io::Result<(S, PeerContext)> {
        if self.state.get::<bool>(&self.key).unwrap_or(false) {
            assert_reachable!("rpc peer dial refused by the one-way gate");
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "outbound dials blocked (one-way reachability)",
            ));
        }
        Ok((stream, PeerContext::new(peer)))
    }
}

impl<S> Acceptor<S> for DialGate
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = S;

    async fn accept(&self, stream: S, peer: &str) -> io::Result<(S, PeerContext)> {
        Ok((stream, PeerContext::new(peer)))
    }
}

fn sharing_label(sharing: InboundSharing) -> u8 {
    match sharing {
        InboundSharing::Disabled => 0,
        InboundSharing::SameIp => 1,
        InboundSharing::Trusted => 2,
    }
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
        // Each peer process draws its own sharing mode, so runs meet mixed
        // settings (half the draws share on the claim alone).
        let sharing = sharing_from(ctx.random().random_range(0..4u8));
        state.publish(&sharing_key(&me), sharing_label(sharing));
        let gate = DialGate::new(state.clone(), &me);
        let (driver, rpc) = RpcDriver::listen_with(
            ctx.providers().clone(),
            &address,
            delivery_config_sharing(Some(sharing)),
            gate,
        )
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
                call_other(&rpc, ctx, &me, other.as_deref()),
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
async fn call_other(
    rpc: &RpcHandle<SimProviders>,
    ctx: &SimContext,
    me: &str,
    other: Option<&str>,
) {
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
        let mine = ctx.state().get::<u8>(&sharing_key(me));
        let theirs = ctx.state().get::<u8>(&sharing_key(other));
        if outcome.is_ok() && mine.is_some() && theirs.is_some() && mine != theirs {
            assert_sometimes!(true, "rpc peers with mixed sharing kept calling each other");
        }
        if let Some(stats) = rpc.stats() {
            assert_sometimes!(
                stats.replaced_connections > 0 && outcome.is_ok(),
                "rpc simultaneous connect resolved to one shared connection"
            );
            assert_sometimes!(
                stats.adopted_connections > 0,
                "rpc accepted session adopted as the peer connection"
            );
            if stats.accepted_over_stalled_dial > 0 && outcome.is_ok() {
                assert_sometimes!(
                    true,
                    "rpc peer reached through its own dial while ours stalled"
                );
            }
            if stats.unverified_listen_addresses > 0 {
                assert_reachable!("rpc unverified listen address served only");
            }
        }
    }
}
