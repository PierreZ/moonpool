//! Hash-chain server process for the transport simulation.

use async_trait::async_trait;
use tracing::instrument;

use moonpool_sim::{Process, SimContext, SimulationResult, assert_sometimes, buggify};
use moonpool_transport::{NetTransport, NetTransportBuilder, ReplyPromise};

use crate::hash::{INITIAL_DIGEST, fold};
use crate::service::{
    APPEND_INTERFACE, AppendBlockRequest, AppendBlockResponse, METHOD_APPEND_BLOCK, parse_sim_addr,
};

/// Trail name for server-side append events. The integrity invariant replays
/// from this trail.
pub const TL_APPEND: &str = "append";

/// Event emitted per successful append. Includes the block bytes so the
/// invariant can replay the chain end-to-end from `(0, INITIAL_DIGEST)`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, valuable::Valuable)]
pub struct AppendBlockEvent {
    /// Block count after this append (`N` post-transition).
    pub n: u64,
    /// Chain digest after this append (`H` post-transition).
    pub h: u64,
    /// Block bytes that were folded in.
    pub block: Vec<u8>,
    /// IP of the server that produced this event.
    pub server_ip: String,
}

/// Hash-chain server. State is in-memory only and resets on every boot.
pub struct TransportServerProcess;

#[async_trait]
impl Process for TransportServerProcess {
    fn name(&self) -> &'static str {
        "transport-server"
    }

    #[instrument(skip(self, ctx))]
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let my_ip = ctx.my_ip();
        let addr = parse_sim_addr(my_ip)?;

        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(addr)
            .build_listening()
            .await
            .map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("transport build: {e}"))
            })?;

        let (append_stream, _) = NetTransport::register_handler_at::<
            AppendBlockRequest,
            AppendBlockResponse,
        >(&transport, APPEND_INTERFACE, METHOD_APPEND_BLOCK);

        // Hash-chain state lives on the run() stack — fresh on every boot.
        let mut h: u64 = INITIAL_DIGEST;
        let mut n: u64 = 0;

        tracing::info!(%my_ip, "transport server started");
        let shutdown = ctx.shutdown().clone();

        loop {
            tokio::select! {
                Some((req, reply)) = append_stream.recv() => {
                    handle_append(&mut h, &mut n, &req, reply, ctx, my_ip);
                }
                () = shutdown.cancelled() => {
                    tracing::info!(final_n = n, final_h = h, "transport server shutting down");
                    return Ok(());
                }
            }
        }
    }
}

fn handle_append(
    h: &mut u64,
    n: &mut u64,
    req: &AppendBlockRequest,
    reply: ReplyPromise<AppendBlockResponse>,
    ctx: &SimContext,
    server_ip: &str,
) {
    // Drop the reply mid-RPC without mutating state. Simulates "server crashed
    // between receive and commit" and forces the transport's at-least-once
    // retry path. If the transport double-delivers a retry and we commit twice,
    // N advances past the workload's expected_n+1 on the next op and the
    // reference-model check fails.
    if buggify!() {
        assert_sometimes!(true, "server_buggify_dropped_promise");
        drop(reply);
        return;
    }

    let new_h = fold(*h, &req.block);
    let new_n = n
        .checked_add(1)
        .expect("N overflow impossible in 10s chaos_duration");
    *h = new_h;
    *n = new_n;

    ctx.emit(
        TL_APPEND,
        AppendBlockEvent {
            n: new_n,
            h: new_h,
            block: req.block.clone(),
            server_ip: server_ip.to_string(),
        },
    );

    assert_sometimes!(req.block.is_empty(), "handled_empty_block");
    assert_sometimes!(req.block.len() >= 60, "handled_large_block");

    reply.send(AppendBlockResponse {
        seq_id: req.seq_id,
        n: new_n,
        h: new_h,
        server_ip: server_ip.to_string(),
    });
}
