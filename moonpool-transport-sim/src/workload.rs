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
use serde::{Deserialize, Serialize};
use tracing::instrument;

use moonpool_sim::{
    SimContext, SimulationResult, Workload, assert_always, assert_sometimes, sim_random_range,
};
use moonpool_transport::{
    Endpoint, JsonCodec, NetTransportBuilder, Providers, ReplyError, TimeProvider, UID, get_reply,
    get_reply_unless_failed_for, make_decode_fn, make_encode_fn, send, try_get_reply,
};

use crate::hash::{INITIAL_DIGEST, fold};
use crate::process::{AppendBlockEvent, TL_APPEND};
use crate::service::{
    AppendBlockRequest, AppendBlockResponse, METHOD_APPEND_BLOCK, append_method_uid, parse_sim_addr,
};

/// Timeline key for client-side append attempts and outcomes.
pub const TL_CLIENT: &str = "client";

/// Client-side timeline event recording the outcome of each attempted append.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientEvent {
    /// Request issued; outcome not yet known.
    Issued {
        /// Workload-side request id.
        seq_id: u64,
        /// Number of bytes in the block.
        block_len: usize,
    },
    /// Server returned a response; compared bit-for-bit against the reference model.
    Acknowledged {
        /// Workload-side request id.
        seq_id: u64,
        /// `n` reported by the server.
        server_n: u64,
        /// `h` reported by the server.
        server_h: u64,
        /// `n` projected by the local reference model.
        expected_n: u64,
        /// `h` projected by the local reference model.
        expected_h: u64,
    },
    /// At-least-once RPC failed terminally.
    Failed {
        /// Workload-side request id.
        seq_id: u64,
        /// Stringified error.
        error: String,
    },
    /// Ambiguous outcome (`MaybeDelivered`, drain timeout, or shutdown mid-await).
    Ambiguous {
        /// Workload-side request id.
        seq_id: u64,
    },
}

#[derive(Debug, Clone, Copy)]
enum Op {
    NormalAppend,
    EmptyBlock,
    MaxSizeBlock,
    AtMostOnceAppend,
    AppendWithTimeout,
    SendToWrongEndpoint,
    SmallDelay,
}

fn random_op() -> Op {
    match sim_random_range(0u32..100) {
        0..70 => Op::NormalAppend,
        70..78 => Op::EmptyBlock,
        78..85 => Op::MaxSizeBlock,
        85..91 => Op::AtMostOnceAppend,
        91..96 => Op::AppendWithTimeout,
        96..98 => Op::SendToWrongEndpoint,
        _ => Op::SmallDelay,
    }
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

        tracing::info!(client_id, num_ops, "workload starting");

        for _ in 0..num_ops {
            if shutdown.is_cancelled() || stopped_after_ambiguous {
                break;
            }

            let op = random_op();
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

            ctx.emit(
                TL_CLIENT,
                ClientEvent::Issued {
                    seq_id,
                    block_len: block.len(),
                },
            );

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
                            let drained: Option<Result<AppendBlockResponse, ReplyError>> = tokio::select! {
                                r = fut => Some(r),
                                () = shutdown.cancelled() => None,
                                _ = time.sleep(Duration::from_secs(10)) => None,
                            };
                            if let Some(r) = drained {
                                r
                            } else {
                                ctx.emit(TL_CLIENT, ClientEvent::Ambiguous { seq_id });
                                stopped_after_ambiguous = true;
                                continue;
                            }
                        }
                        Err(e) => {
                            ctx.emit(
                                TL_CLIENT,
                                ClientEvent::Failed {
                                    seq_id,
                                    error: e.to_string(),
                                },
                            );
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

                    ctx.emit(
                        TL_CLIENT,
                        ClientEvent::Acknowledged {
                            seq_id,
                            server_n: resp.n,
                            server_h: resp.h,
                            expected_n: projected_n,
                            expected_h: projected_h,
                        },
                    );
                }
                Err(ReplyError::MaybeDelivered) => {
                    assert_sometimes!(true, "maybe_delivered_observed");
                    ctx.emit(TL_CLIENT, ClientEvent::Ambiguous { seq_id });
                    stopped_after_ambiguous = true;
                }
                Err(e) => {
                    assert_sometimes!(true, "terminal_error_observed");
                    ctx.emit(
                        TL_CLIENT,
                        ClientEvent::Failed {
                            seq_id,
                            error: e.to_string(),
                        },
                    );
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
                ctx.emit(
                    TL_CLIENT,
                    ClientEvent::Issued {
                        seq_id,
                        block_len: block.len(),
                    },
                );
                match send(&*transport, &endpoint, req, &encode) {
                    Ok(()) => {
                        assert_sometimes!(true, "fire_and_forget_sent");
                        ctx.emit(TL_CLIENT, ClientEvent::Ambiguous { seq_id });
                    }
                    Err(e) => {
                        ctx.emit(
                            TL_CLIENT,
                            ClientEvent::Failed {
                                seq_id,
                                error: e.to_string(),
                            },
                        );
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
        // Every Acknowledged event must have a matching server-side AppendBlockEvent
        // with the same (n, h). Catches "transport claimed delivery, server never
        // emitted" — a lost server-side event.
        let client_events = ctx.timeline::<ClientEvent>(TL_CLIENT);
        let server_events = ctx.timeline::<AppendBlockEvent>(TL_APPEND);
        for entry in &client_events {
            if let ClientEvent::Acknowledged {
                server_n, server_h, ..
            } = &entry.event
            {
                let matched = server_events
                    .iter()
                    .any(|s| s.event.n == *server_n && s.event.h == *server_h);
                assert_always!(matched, "check_ack_has_matching_server_event");
            }
        }
        tracing::info!("check passed");
        Ok(())
    }
}
