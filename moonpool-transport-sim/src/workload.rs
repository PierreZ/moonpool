//! Hash-chain client workload that exercises moonpool-transport end-to-end.
//!
//! The workload maintains an exact local reference model — `(expected_n, expected_h)` —
//! and precomputes what the server's response *must* be before each request. Any
//! transport bug that mutates, reorders, drops, or duplicates a request/response
//! produces a deterministic `assert_always!` failure with a reproducible seed.
//!
//! Pedagogical demo: mistype [`fold`] in either the server or the workload so the
//! two diverge by one bit — the very first append after the mistype trips
//! `response_h_matches_reference_model` with a reproducible seed.

use std::time::Duration;

use async_trait::async_trait;
use tracing::instrument;

use moonpool_sim::{
    SimContext, SimulationResult, TraceQuery, Workload, assert_always, assert_sometimes,
    sim_random_range,
};
use moonpool_transport::{
    Endpoint, JsonCodec, NetTransportBuilder, Providers, ReplyError, TimeProvider, UID, get_reply,
    get_reply_unless_failed_for, make_decode_fn, make_encode_fn, send, try_get_reply,
};

use crate::hash::{INITIAL_DIGEST, fold};
use crate::process::EV_APPEND_BLOCK;
use crate::service::{
    AppendBlockRequest, AppendBlockResponse, METHOD_APPEND_BLOCK, append_method_uid, parse_sim_addr,
};

/// Event name: request issued; outcome not yet known. Fields: `seq_id`,
/// `block_len`.
pub const EV_CLIENT_ISSUED: &str = "client_issued";

/// Event name: server response compared bit-for-bit against the reference
/// model. Fields: `seq_id`, `server_n`, `server_h`, `expected_n`,
/// `expected_h`.
pub const EV_CLIENT_ACKNOWLEDGED: &str = "client_acknowledged";

/// Event name: at-least-once RPC failed terminally. Fields: `seq_id`,
/// `error`.
pub const EV_CLIENT_FAILED: &str = "client_failed";

/// Event name: ambiguous outcome (`MaybeDelivered`, drain timeout, or
/// shutdown mid-await). Fields: `seq_id`.
pub const EV_CLIENT_AMBIGUOUS: &str = "client_ambiguous";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    NormalAppend,
    EmptyBlock,
    MaxSizeBlock,
    AtMostOnceAppend,
    AppendWithTimeout,
    SendToWrongEndpoint,
    SmallDelay,
}

impl Op {
    /// True for operations that issue a request the server turns into an
    /// `append_block` (i.e. that can advance the hash chain).
    fn is_append(self) -> bool {
        matches!(
            self,
            Op::NormalAppend
                | Op::EmptyBlock
                | Op::MaxSizeBlock
                | Op::AtMostOnceAppend
                | Op::AppendWithTimeout
        )
    }
}

/// The operation alphabet with its baseline weights (summing to 100). The array
/// index is the stable operation id consulted by [`moonpool_sim::swarm_op_enabled`].
const OP_ALPHABET: [(Op, u32); 7] = [
    (Op::NormalAppend, 70),
    (Op::EmptyBlock, 8),
    (Op::MaxSizeBlock, 7),
    (Op::AtMostOnceAppend, 6),
    (Op::AppendWithTimeout, 5),
    (Op::SendToWrongEndpoint, 2),
    (Op::SmallDelay, 2),
];

/// This seed's enabled operation subset for swarm testing. Each operation is
/// independently ~50% on (via the per-seed mask); with swarm disabled every
/// operation is enabled, so the table is byte-identical to [`OP_ALPHABET`].
/// Empty-mask fallback: if a seed disables everything, use the full alphabet so
/// the workload always has something to do.
fn swarm_enabled_ops() -> Vec<(Op, u32)> {
    let enabled: Vec<(Op, u32)> = (0u8..)
        .zip(OP_ALPHABET)
        .filter(|&(id, _)| moonpool_sim::swarm_op_enabled(id))
        .map(|(_, op)| op)
        .collect();
    if enabled.is_empty() {
        OP_ALPHABET.to_vec()
    } else {
        enabled
    }
}

/// Pick an operation from the enabled subset, weighted by `table`. Consumes
/// exactly one `SIM_RNG` draw per call (the remapping into the subset adds no
/// draws, so fork-explorer replay is unperturbed).
fn weighted_op(table: &[(Op, u32)]) -> Op {
    let total: u32 = table.iter().map(|&(_, w)| w).sum();
    let mut roll = sim_random_range(0u32..total);
    for &(op, w) in table {
        if roll < w {
            return op;
        }
        roll -= w;
    }
    table.last().expect("enabled op table is never empty").0
}

fn random_block(min: usize, max: usize) -> Vec<u8> {
    let len = sim_random_range(min..max.saturating_add(1));
    (0..len)
        .map(|_| {
            let v: u32 = sim_random_range(0u32..256);
            u8::try_from(v).expect("v < 256 by construction")
        })
        .collect()
}

/// Hash-chain client workload. One instance per simulation (1 server + 1 workload).
pub struct TransportClientWorkload {
    index: usize,
}

impl TransportClientWorkload {
    /// Create a new workload with the given instance index.
    #[must_use]
    pub fn new(index: usize) -> Self {
        Self { index }
    }
}

#[async_trait]
impl Workload for TransportClientWorkload {
    fn name(&self) -> &'static str {
        "transport-client"
    }

    #[instrument(skip(self, ctx), fields(client = self.index))]
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let servers = ctx.topology().all_process_ips().to_vec();
        if servers.is_empty() {
            tracing::warn!("no servers found, skipping workload");
            return Ok(());
        }

        // Topology is 1 server — pick the first deterministically.
        let server_ip = servers[0].clone();
        let my_addr = parse_sim_addr(ctx.my_ip())?;

        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(my_addr)
            .build_listening()
            .await
            .map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("transport build: {e}"))
            })?;

        let endpoint = Endpoint::new(
            parse_sim_addr(&server_ip)?,
            append_method_uid(METHOD_APPEND_BLOCK),
        );
        let bad_endpoint = Endpoint::new(
            parse_sim_addr(&server_ip)?,
            UID::new(0xDEAD_BEEF, 0xBAD0_BAD0),
        );

        let encode =
            make_encode_fn::<moonpool_transport::RequestEnvelope<AppendBlockRequest>, _>(JsonCodec);
        let decode = make_decode_fn::<Result<AppendBlockResponse, ReplyError>, _>(JsonCodec);

        let client_id = ctx.client_id();
        let num_ops = sim_random_range(20u64..200);
        let shutdown = ctx.shutdown().clone();
        let time = ctx.providers().time().clone();
        let mut seq_counter: u64 = 0;

        // Reference model: locally-projected expected (N, H). With a single client
        // this matches server-side truth bit-for-bit until an ambiguous outcome,
        // at which point we stop driving and let the invariant be the oracle.
        let mut expected_n: u64 = 0;
        let mut expected_h: u64 = INITIAL_DIGEST;
        let mut stopped_after_ambiguous = false;

        // Per-seed operation-alphabet subset (swarm testing). Computed once;
        // `swarm_op_enabled` is a pure function of the seed, so this is stable.
        let op_table = swarm_enabled_ops();

        // Demonstrator: this seed masked off every append-producing op, so the
        // hash chain can never advance — only delays / wrong-endpoint probes run.
        // Reachable under swarm (~(1/2)^5 per seed), impossible under all-on.
        if !op_table.iter().any(|&(op, _)| op.is_append()) {
            assert_sometimes!(true, "swarm: no append ops, chain cannot advance");
        }

        tracing::info!(client_id, num_ops, "workload starting");

        for _ in 0..num_ops {
            if shutdown.is_cancelled() || stopped_after_ambiguous {
                break;
            }

            let op = weighted_op(&op_table);
            seq_counter += 1;
            let seq_id = (client_id as u64) * 1_000_000 + seq_counter;

            match op {
                Op::SmallDelay => {
                    let ms = sim_random_range(1u64..50);
                    let _ = time.sleep(Duration::from_millis(ms)).await;
                    continue;
                }
                Op::SendToWrongEndpoint => {
                    let req = AppendBlockRequest {
                        seq_id,
                        client_id,
                        block: vec![0u8; 1],
                    };
                    let result =
                        try_get_reply(&*transport, &bad_endpoint, req, &encode, decode.clone())
                            .await;
                    assert_always!(result.is_err(), "wrong_endpoint_should_not_succeed");
                    assert_sometimes!(true, "wrong_endpoint_returns_error");
                    continue;
                }
                _ => {}
            }

            let block: Vec<u8> = match op {
                Op::NormalAppend | Op::AtMostOnceAppend | Op::AppendWithTimeout => {
                    random_block(1, 64)
                }
                Op::EmptyBlock => Vec::new(),
                Op::MaxSizeBlock => random_block(64, 64),
                Op::SmallDelay | Op::SendToWrongEndpoint => unreachable!("handled above"),
            };

            let projected_n = expected_n + 1;
            let projected_h = fold(expected_h, &block);

            tracing::info!(seq_id, block_len = block.len(), "client_issued");

            let req = AppendBlockRequest {
                seq_id,
                client_id,
                block: block.clone(),
            };

            // Drive the request through the chosen delivery mode.
            let result: Result<AppendBlockResponse, ReplyError> = match op {
                Op::NormalAppend | Op::EmptyBlock | Op::MaxSizeBlock => {
                    match get_reply(&*transport, &endpoint, req, &encode, decode.clone()) {
                        Ok(fut) => {
                            let drained: Option<Result<AppendBlockResponse, ReplyError>> = moonpool_sim::select! {
                                biased;
                                r = fut => Some(r),
                                () = shutdown.cancelled() => None,
                                _ = time.sleep(Duration::from_secs(10)) => None,
                            };
                            if let Some(r) = drained {
                                r
                            } else {
                                tracing::info!(seq_id, "client_ambiguous");
                                stopped_after_ambiguous = true;
                                continue;
                            }
                        }
                        Err(e) => {
                            tracing::info!(seq_id, error = %e, "client_failed");
                            continue;
                        }
                    }
                }
                Op::AtMostOnceAppend => {
                    try_get_reply(&*transport, &endpoint, req, &encode, decode.clone()).await
                }
                Op::AppendWithTimeout => {
                    let dur = Duration::from_millis(sim_random_range(50u64..5_000));
                    get_reply_unless_failed_for(
                        &*transport,
                        &endpoint,
                        req,
                        &encode,
                        decode.clone(),
                        dur,
                    )
                    .await
                }
                Op::SmallDelay | Op::SendToWrongEndpoint => unreachable!("handled above"),
            };

            match result {
                Ok(resp) => {
                    assert_always!(resp.seq_id == seq_id, "response_seq_matches");
                    assert_always!(resp.n == projected_n, "response_n_matches_reference_model");
                    assert_always!(resp.h == projected_h, "response_h_matches_reference_model");
                    assert_sometimes!(true, "reply_received");

                    expected_n = projected_n;
                    expected_h = projected_h;

                    tracing::info!(
                        seq_id,
                        server_n = resp.n,
                        server_h = resp.h,
                        expected_n = projected_n,
                        expected_h = projected_h,
                        "client_acknowledged"
                    );
                }
                Err(ReplyError::MaybeDelivered) => {
                    assert_sometimes!(true, "maybe_delivered_observed");
                    tracing::info!(seq_id, "client_ambiguous");
                    stopped_after_ambiguous = true;
                }
                Err(e) => {
                    assert_sometimes!(true, "terminal_error_observed");
                    tracing::info!(seq_id, error = %e, "client_failed");
                }
            }
        }

        // Phase 2: fire-and-forget burst. `send()` produces no reply, so we
        // can't update the reference model from it. The integrity invariant
        // remains the oracle for any FF commits. Drain-aware: respects shutdown.
        if !shutdown.is_cancelled() {
            let ff_count = sim_random_range(0u64..10);
            for _ in 0..ff_count {
                if shutdown.is_cancelled() {
                    break;
                }
                seq_counter += 1;
                let seq_id = (client_id as u64) * 1_000_000 + seq_counter;
                let block = random_block(1, 64);
                let req = AppendBlockRequest {
                    seq_id,
                    client_id,
                    block: block.clone(),
                };
                tracing::info!(seq_id, block_len = block.len(), "client_issued");
                match send(&*transport, &endpoint, req, &encode) {
                    Ok(()) => {
                        assert_sometimes!(true, "fire_and_forget_sent");
                        tracing::info!(seq_id, "client_ambiguous");
                    }
                    Err(e) => {
                        tracing::info!(seq_id, error = %e, "client_failed");
                    }
                }
            }
        }

        ctx.state().publish(
            &format!("transport_client_state_{client_id}"),
            (expected_n, expected_h),
        );

        tracing::info!(client_id, expected_n, expected_h, "workload finished");
        Ok(())
    }

    #[instrument(skip(self, ctx), fields(client = self.index))]
    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Every acknowledged append must have a matching server-side
        // `append_block` event with the same (n, h). Catches "transport
        // claimed delivery, server never emitted" — a lost server-side event.
        let q = ctx.observability();
        let acks = q.snapshot(EV_CLIENT_ACKNOWLEDGED);
        let server_events = q.snapshot(EV_APPEND_BLOCK);
        for ack in &acks {
            let server_n = ack.u64("server_n");
            let server_h = ack.u64("server_h");
            assert_always!(
                server_n.is_some() && server_h.is_some(),
                "check_ack_event_well_formed"
            );
            let matched = server_events
                .iter()
                .any(|s| s.u64("n") == server_n && s.u64("h") == server_h);
            assert_always!(matched, "check_ack_has_matching_server_event");
        }
        tracing::info!("check passed");
        Ok(())
    }
}
