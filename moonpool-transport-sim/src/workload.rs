//! Client workload that exercises all 4 RPC delivery modes under chaos.
//!
//! Sends random operations to echo servers, emitting timeline events for
//! invariant checking. Drain-aware: stops sending on shutdown, waits for
//! in-flight requests.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use moonpool_sim::{
    SimContext, SimulationResult, Workload, assert_always, assert_sometimes, sim_random_range,
};
use moonpool_transport::{
    Endpoint, JsonCodec, NetTransportBuilder, Providers, ReplyError, TimeProvider, UID, get_reply,
    get_reply_unless_failed_for, send, try_get_reply,
};

use crate::report::WorkloadStats;
use crate::service::{
    DeliveryMode, EchoRequest, EchoResponse, METHOD_ECHO, METHOD_ECHO_DELAYED, METHOD_ECHO_OR_FAIL,
    echo_method_uid, parse_sim_addr,
};

// =============================================================================
// Timeline events
// =============================================================================

/// Events emitted to per-mode timelines for invariant checking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeliveryEvent {
    /// Request sent to a server.
    Sent {
        /// Unique sequence ID.
        seq_id: u64,
        /// Server IP address.
        server: String,
        /// Method index (1, 2, or 3).
        method: u64,
    },
    /// Response received successfully.
    Replied {
        /// Echoed sequence ID.
        seq_id: u64,
    },
    /// Request failed with an error.
    Failed {
        /// Sequence ID of the failed request.
        seq_id: u64,
        /// Error description.
        error: String,
    },
    /// Request timed out after sustained failure.
    TimedOut {
        /// Sequence ID.
        seq_id: u64,
        /// Timeout duration in milliseconds.
        duration_ms: u64,
    },
    /// Ambiguous outcome: request may or may not have been delivered.
    MaybeDelivered {
        /// Sequence ID.
        seq_id: u64,
    },
}

/// Timeline key for fire-and-forget delivery events.
pub const TL_FIRE_AND_FORGET: &str = "fire_and_forget";
/// Timeline key for at-most-once delivery events.
pub const TL_AT_MOST_ONCE: &str = "at_most_once";
/// Timeline key for at-least-once delivery events.
pub const TL_AT_LEAST_ONCE: &str = "at_least_once";
/// Timeline key for timeout delivery events.
pub const TL_TIMEOUT: &str = "timeout";

// =============================================================================
// Operation alphabet
// =============================================================================

/// Weighted operations for the workload.
#[derive(Debug, Clone, Copy)]
enum Op {
    SendFireAndForget,
    SendAtMostOnce,
    SendAtLeastOnce,
    SendWithTimeout,
    SendToWrongEndpoint,
    SmallDelay,
    CheckMetrics,
}

/// Pick a random operation based on weights.
fn random_op() -> Op {
    let roll = sim_random_range(0..100);
    match roll {
        0..15 => Op::SendFireAndForget,
        15..35 => Op::SendAtMostOnce,
        35..60 => Op::SendAtLeastOnce,
        60..75 => Op::SendWithTimeout,
        75..80 => Op::SendToWrongEndpoint,
        80..90 => Op::SmallDelay,
        _ => Op::CheckMetrics,
    }
}

/// Pick a random method index (1, 2, or 3).
fn random_method() -> u64 {
    match sim_random_range(0..3) {
        0 => METHOD_ECHO,
        1 => METHOD_ECHO_DELAYED,
        _ => METHOD_ECHO_OR_FAIL,
    }
}

/// Pick a random server from the list.
fn random_server(servers: &[String]) -> &str {
    let idx = sim_random_range(0..servers.len());
    &servers[idx]
}

// =============================================================================
// Workload
// =============================================================================

/// Transport client workload that exercises all delivery modes.
pub struct TransportClientWorkload {
    /// Instance index for naming.
    index: usize,
}

impl TransportClientWorkload {
    /// Create a new workload with the given instance index.
    pub fn new(index: usize) -> Self {
        Self { index }
    }
}

#[async_trait(?Send)]
impl Workload for TransportClientWorkload {
    fn name(&self) -> &str {
        "transport-client"
    }

    #[instrument(skip(self, ctx), fields(client = self.index))]
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let servers = ctx.topology().all_process_ips().to_vec();
        if servers.is_empty() {
            tracing::warn!("no servers found, skipping workload");
            return Ok(());
        }

        let my_addr = parse_sim_addr(ctx.my_ip())?;

        // Build transport with listener (needed for RPC responses)
        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(my_addr)
            .build_listening()
            .await
            .map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("transport build: {e}"))
            })?;

        let client_id = ctx.client_id();
        let num_ops = sim_random_range(50..200);
        let mut seq_counter: u64 = 0;
        let shutdown = ctx.shutdown().clone();

        let stats = Rc::new(RefCell::new(WorkloadStats::default()));

        tracing::info!(
            client_id,
            num_ops,
            num_servers = servers.len(),
            "workload starting"
        );

        for _op_idx in 0..num_ops {
            if shutdown.is_cancelled() {
                tracing::info!("shutdown received, stopping new operations");
                break;
            }

            let op = random_op();
            seq_counter += 1;
            let seq_id = (client_id as u64) * 1_000_000 + seq_counter;

            match op {
                Op::SendFireAndForget => {
                    let server = random_server(&servers);
                    let method = random_method();
                    let server_addr = parse_sim_addr(server)?;
                    let endpoint = Endpoint::new(server_addr, echo_method_uid(method));

                    let req = EchoRequest {
                        seq_id,
                        client_id,
                        mode: DeliveryMode::FireAndForget,
                        method,
                    };

                    ctx.emit(
                        TL_FIRE_AND_FORGET,
                        DeliveryEvent::Sent {
                            seq_id,
                            server: server.to_string(),
                            method,
                        },
                    );

                    match send(&transport, &endpoint, req, JsonCodec) {
                        Ok(()) => {
                            stats.borrow_mut().fire_and_forget_sent += 1;
                        }
                        Err(e) => {
                            ctx.emit(
                                TL_FIRE_AND_FORGET,
                                DeliveryEvent::Failed {
                                    seq_id,
                                    error: e.to_string(),
                                },
                            );
                            stats.borrow_mut().fire_and_forget_errors += 1;
                        }
                    }
                }

                Op::SendAtMostOnce => {
                    let server = random_server(&servers).to_string();
                    let method = random_method();
                    let server_addr = parse_sim_addr(&server)?;
                    let endpoint = Endpoint::new(server_addr, echo_method_uid(method));

                    let req = EchoRequest {
                        seq_id,
                        client_id,
                        mode: DeliveryMode::AtMostOnce,
                        method,
                    };

                    ctx.emit(
                        TL_AT_MOST_ONCE,
                        DeliveryEvent::Sent {
                            seq_id,
                            server: server.clone(),
                            method,
                        },
                    );

                    stats.borrow_mut().at_most_once_sent += 1;

                    match try_get_reply::<_, EchoResponse, _, _>(
                        &transport, &endpoint, req, JsonCodec,
                    )
                    .await
                    {
                        Ok(resp) => {
                            assert_always!(
                                resp.seq_id == seq_id,
                                "at_most_once_response_matches_request"
                            );
                            ctx.emit(TL_AT_MOST_ONCE, DeliveryEvent::Replied { seq_id });
                            stats.borrow_mut().at_most_once_replied += 1;
                        }
                        Err(ReplyError::MaybeDelivered) => {
                            assert_sometimes!(true, "at_most_once_maybe_delivered");
                            ctx.emit(TL_AT_MOST_ONCE, DeliveryEvent::MaybeDelivered { seq_id });
                            stats.borrow_mut().at_most_once_maybe += 1;
                        }
                        Err(e) => {
                            ctx.emit(
                                TL_AT_MOST_ONCE,
                                DeliveryEvent::Failed {
                                    seq_id,
                                    error: e.to_string(),
                                },
                            );
                            stats.borrow_mut().at_most_once_errors += 1;
                        }
                    }
                }

                Op::SendAtLeastOnce => {
                    let server = random_server(&servers).to_string();
                    let method = random_method();
                    let server_addr = parse_sim_addr(&server)?;
                    let endpoint = Endpoint::new(server_addr, echo_method_uid(method));

                    let req = EchoRequest {
                        seq_id,
                        client_id,
                        mode: DeliveryMode::AtLeastOnce,
                        method,
                    };

                    ctx.emit(
                        TL_AT_LEAST_ONCE,
                        DeliveryEvent::Sent {
                            seq_id,
                            server: server.clone(),
                            method,
                        },
                    );

                    stats.borrow_mut().at_least_once_sent += 1;

                    match get_reply::<_, EchoResponse, _, _>(&transport, &endpoint, req, JsonCodec)
                    {
                        Ok(reply_future) => {
                            let time = ctx.providers().time().clone();
                            let result: Option<Result<EchoResponse, ReplyError>> = tokio::select! {
                                r = reply_future => Some(r),
                                _ = shutdown.cancelled() => {
                                    tracing::debug!(seq_id, "at_least_once drain timeout");
                                    None
                                }
                                _ = time.sleep(Duration::from_secs(10)) => {
                                    tracing::debug!(seq_id, "at_least_once timed out waiting");
                                    None
                                }
                            };

                            match result {
                                Some(Ok(resp)) => {
                                    assert_always!(
                                        resp.seq_id == seq_id,
                                        "at_least_once_response_matches_request"
                                    );
                                    ctx.emit(TL_AT_LEAST_ONCE, DeliveryEvent::Replied { seq_id });
                                    stats.borrow_mut().at_least_once_replied += 1;
                                }
                                Some(Err(e)) => {
                                    ctx.emit(
                                        TL_AT_LEAST_ONCE,
                                        DeliveryEvent::Failed {
                                            seq_id,
                                            error: e.to_string(),
                                        },
                                    );
                                    stats.borrow_mut().at_least_once_errors += 1;
                                }
                                None => {
                                    stats.borrow_mut().at_least_once_in_flight += 1;
                                }
                            }
                        }
                        Err(e) => {
                            ctx.emit(
                                TL_AT_LEAST_ONCE,
                                DeliveryEvent::Failed {
                                    seq_id,
                                    error: e.to_string(),
                                },
                            );
                            stats.borrow_mut().at_least_once_errors += 1;
                        }
                    }
                }

                Op::SendWithTimeout => {
                    let server = random_server(&servers).to_string();
                    let method = random_method();
                    let server_addr = parse_sim_addr(&server)?;
                    let endpoint = Endpoint::new(server_addr, echo_method_uid(method));

                    let timeout_ms = sim_random_range(100..10_000) as u64;
                    let timeout_dur = Duration::from_millis(timeout_ms);

                    let req = EchoRequest {
                        seq_id,
                        client_id,
                        mode: DeliveryMode::Timeout,
                        method,
                    };

                    ctx.emit(
                        TL_TIMEOUT,
                        DeliveryEvent::Sent {
                            seq_id,
                            server: server.clone(),
                            method,
                        },
                    );

                    stats.borrow_mut().timeout_sent += 1;

                    match get_reply_unless_failed_for::<_, EchoResponse, _, _>(
                        &transport,
                        &endpoint,
                        req,
                        JsonCodec,
                        timeout_dur,
                    )
                    .await
                    {
                        Ok(resp) => {
                            assert_always!(
                                resp.seq_id == seq_id,
                                "timeout_response_matches_request"
                            );
                            ctx.emit(TL_TIMEOUT, DeliveryEvent::Replied { seq_id });
                            stats.borrow_mut().timeout_replied += 1;
                        }
                        Err(ReplyError::MaybeDelivered) => {
                            assert_sometimes!(true, "timeout_maybe_delivered");
                            ctx.emit(
                                TL_TIMEOUT,
                                DeliveryEvent::TimedOut {
                                    seq_id,
                                    duration_ms: timeout_ms,
                                },
                            );
                            stats.borrow_mut().timeout_timed_out += 1;
                        }
                        Err(e) => {
                            ctx.emit(
                                TL_TIMEOUT,
                                DeliveryEvent::Failed {
                                    seq_id,
                                    error: e.to_string(),
                                },
                            );
                            stats.borrow_mut().timeout_errors += 1;
                        }
                    }
                }

                Op::SendToWrongEndpoint => {
                    let server = random_server(&servers).to_string();
                    let server_addr = parse_sim_addr(&server)?;
                    let bad_uid = UID::new(0xDEAD_BEEF, 0xBAD0_BAD0);
                    let endpoint = Endpoint::new(server_addr, bad_uid);

                    let req = EchoRequest {
                        seq_id,
                        client_id,
                        mode: DeliveryMode::AtMostOnce,
                        method: 99,
                    };

                    match try_get_reply::<_, EchoResponse, _, _>(
                        &transport, &endpoint, req, JsonCodec,
                    )
                    .await
                    {
                        Ok(_) => {
                            assert_always!(false, "wrong_endpoint_should_not_succeed");
                        }
                        Err(_) => {
                            assert_sometimes!(true, "wrong_endpoint_returns_error");
                            stats.borrow_mut().endpoint_not_found += 1;
                        }
                    }
                }

                Op::SmallDelay => {
                    let delay_ms = sim_random_range(1..50) as u64;
                    let _ = ctx
                        .providers()
                        .time()
                        .sleep(Duration::from_millis(delay_ms))
                        .await;
                }

                Op::CheckMetrics => {
                    let s = stats.borrow();
                    tracing::debug!(
                        client_id,
                        ff_sent = s.fire_and_forget_sent,
                        amo_sent = s.at_most_once_sent,
                        alo_sent = s.at_least_once_sent,
                        to_sent = s.timeout_sent,
                        "metrics checkpoint"
                    );
                }
            }
        }

        // Publish stats for report
        let final_stats = stats.borrow().clone();
        let key = format!("transport_stats_{}", client_id);
        ctx.state().publish(&key, final_stats);

        tracing::info!(client_id, "workload finished");
        Ok(())
    }
}
