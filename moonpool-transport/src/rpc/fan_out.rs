//! Fan-out primitives: issue the same request to N peers in parallel.
//!
//! This module provides four completion semantics, each modelled on a
//! different FoundationDB pattern:
//!
//! | Function | Completion | FDB analog |
//! |----------|-----------|------------|
//! | [`fan_out_all`] | All-must-succeed | Resolver fan-out (`getAll(replies)` in `CommitProxyServer.actor.cpp:1127-1179`) |
//! | `fan_out_quorum` | K-of-N quorum | TLog commit fan-out (`quorum(N, N - antiQuorum)` in `TagPartitionedLogSystem.actor.cpp:619-687`) — added in a later commit |
//! | `fan_out_race` | First-success race | `waitForAny` (`flow/genericactors.actor.h`) — added in a later commit |
//! | `fan_out_all_partial` | Wait-for-all, return per-peer results | (no FDB analog — added for diagnostics) |
//!
//! All four take `Req: Clone` because the caller's request is cloned once per
//! peer at dispatch time.
//!
//! # No tokio spawning
//!
//! These helpers compose futures via [`futures::future::try_join_all`] /
//! `join_all` rather than spawning tasks. This is required by the
//! single-threaded runtime contract — see `CLAUDE.md`. Cancelling losing
//! requests works by dropping the unfinished futures when the helper returns.

use futures::future::{join_all, try_join_all};
use futures::stream::{FuturesUnordered, StreamExt};
use moonpool_sim::assert_sometimes;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::instrument;

use crate::rpc::{RpcError, ServiceEndpoint};
use crate::{MessageCodec, NetTransport, Providers};

/// Errors returned by the fan-out helpers.
#[derive(Debug, thiserror::Error)]
pub enum FanOutError {
    /// The endpoint slice was empty — there was nothing to fan out to.
    #[error("empty endpoint set")]
    Empty,
    /// One or more peers failed and the variant requires every peer to
    /// succeed.
    ///
    /// `errors` contains the per-peer error(s) collected so far. For
    /// short-circuiting variants ([`fan_out_all`]) this is exactly one
    /// element — the first failure that aborted the fan-out.
    #[error("fan-out failed: {} peer error(s)", errors.len())]
    AllFailed {
        /// Per-peer errors that caused the fan-out to abort.
        errors: Vec<RpcError>,
    },
    /// Fewer than `required` peers replied successfully.
    #[error("not enough successful replies: needed {required}, got {received}")]
    QuorumNotMet {
        /// Number of successful replies required.
        required: usize,
        /// Number of successful replies actually received.
        received: usize,
        /// Errors from peers that failed.
        errors: Vec<RpcError>,
    },
}

/// Issue `request` to every endpoint in `endpoints` and wait for **all**
/// replies before returning.
///
/// On the first peer failure the in-flight requests to the remaining peers
/// are dropped (the futures are not polled further). This mirrors FDB's
/// resolver fan-out, where any single resolver failure aborts the commit.
///
/// The returned `Vec<Resp>` is in the **same order as the input
/// `endpoints`**, so callers can correlate replies with senders by index.
///
/// # Errors
///
/// - [`FanOutError::Empty`] if `endpoints` is empty.
/// - [`FanOutError::AllFailed`] (with the single triggering error) if any
///   peer fails. Use [`fan_out_all_partial`] (added in a later commit) if
///   you need every peer's outcome regardless of failures.
#[instrument(skip_all, fields(n = endpoints.len()))]
pub async fn fan_out_all<Req, Resp, P, C>(
    transport: &NetTransport<P>,
    endpoints: &[ServiceEndpoint<Req, Resp, C>],
    request: Req,
) -> Result<Vec<Resp>, FanOutError>
where
    Req: Serialize + Clone,
    Resp: DeserializeOwned + 'static,
    P: Providers,
    C: MessageCodec + Clone,
{
    if endpoints.is_empty() {
        return Err(FanOutError::Empty);
    }

    let futures: Vec<_> = endpoints
        .iter()
        .map(|ep| ep.get_reply(transport, request.clone()))
        .collect();

    match try_join_all(futures).await {
        Ok(replies) => {
            assert_sometimes!(true, "fan_out_all_succeeded");
            Ok(replies)
        }
        Err(err) => {
            assert_sometimes!(true, "fan_out_all_aborted_on_peer_failure");
            Err(FanOutError::AllFailed { errors: vec![err] })
        }
    }
}

/// Issue `request` to every endpoint and return as soon as `required` of them
/// reply successfully.
///
/// Mirrors FDB's TLog commit fan-out (`quorum(N, N - antiQuorum)` in
/// `fdbserver/TagPartitionedLogSystem.actor.cpp:619-687`). The function
/// resolves in one of three ways:
///
/// 1. **Success** — `required` peers replied. The returned `Vec<Resp>` holds
///    the first `required` responses, **in completion order** (not input
///    order). Remaining in-flight futures are dropped (cancelled).
/// 2. **Quorum impossible** — `len(endpoints) - required + 1` peers have
///    failed, so even if every remaining peer succeeded the threshold could
///    not be reached. Returns [`FanOutError::QuorumNotMet`] with all errors
///    collected so far.
/// 3. **Stream exhausted** — every peer responded but successes never reached
///    `required`. Same `QuorumNotMet` error.
///
/// `MaybeDelivered` and `BrokenPromise` errors count exactly like any other
/// failure — fan-out never retries (every peer was already addressed in
/// parallel), so the [`AtMostOnce`](super::AtMostOnce) flag would be a no-op
/// here and is intentionally absent from the signature.
///
/// # Errors
///
/// - [`FanOutError::Empty`] if `endpoints` is empty.
/// - [`FanOutError::QuorumNotMet`] if `required > endpoints.len()` (the
///   request is unsatisfiable from the start) or if too many peers fail to
///   ever reach the threshold.
#[instrument(skip_all, fields(n = endpoints.len(), required))]
pub async fn fan_out_quorum<Req, Resp, P, C>(
    transport: &NetTransport<P>,
    endpoints: &[ServiceEndpoint<Req, Resp, C>],
    request: Req,
    required: usize,
) -> Result<Vec<Resp>, FanOutError>
where
    Req: Serialize + Clone,
    Resp: DeserializeOwned + 'static,
    P: Providers,
    C: MessageCodec + Clone,
{
    if endpoints.is_empty() {
        return Err(FanOutError::Empty);
    }
    let n = endpoints.len();
    if required > n {
        return Err(FanOutError::QuorumNotMet {
            required,
            received: 0,
            errors: Vec::new(),
        });
    }

    let mut pending: FuturesUnordered<_> = endpoints
        .iter()
        .map(|ep| ep.get_reply(transport, request.clone()))
        .collect();

    let mut successes: Vec<Resp> = Vec::with_capacity(required);
    let mut errors: Vec<RpcError> = Vec::new();
    // Once this many peers have failed, the quorum is impossible.
    let max_tolerable_failures = n - required;

    while let Some(result) = pending.next().await {
        match result {
            Ok(resp) => {
                successes.push(resp);
                if successes.len() >= required {
                    assert_sometimes!(true, "fan_out_quorum_threshold_met");
                    return Ok(successes);
                }
            }
            Err(err) => {
                errors.push(err);
                if errors.len() > max_tolerable_failures {
                    assert_sometimes!(true, "fan_out_quorum_threshold_impossible");
                    return Err(FanOutError::QuorumNotMet {
                        required,
                        received: successes.len(),
                        errors,
                    });
                }
            }
        }
    }

    // Stream exhausted without success or definitive failure: not enough
    // successes. (Reachable when required > 0 and the loop drained naturally.)
    Err(FanOutError::QuorumNotMet {
        required,
        received: successes.len(),
        errors,
    })
}

/// Issue `request` to every endpoint and return the **first successful**
/// reply. Pending requests are dropped (cancelled) once a winner is found.
///
/// Mirrors FDB's `waitForAny` (`flow/include/flow/genericactors.actor.h`)
/// applied to a fan-out shape. Useful for hedged reads where the caller wants
/// the lowest-latency reply from a quorum of equivalent peers without
/// touching a [`QueueModel`].
///
/// `MaybeDelivered` and `BrokenPromise` errors count as ordinary failures —
/// `fan_out_race` only returns success if **at least one** peer replies with
/// `Ok`.
///
/// # Errors
///
/// - [`FanOutError::Empty`] if `endpoints` is empty.
/// - [`FanOutError::AllFailed`] (with one error per peer) if every peer
///   fails.
#[instrument(skip_all, fields(n = endpoints.len()))]
pub async fn fan_out_race<Req, Resp, P, C>(
    transport: &NetTransport<P>,
    endpoints: &[ServiceEndpoint<Req, Resp, C>],
    request: Req,
) -> Result<Resp, FanOutError>
where
    Req: Serialize + Clone,
    Resp: DeserializeOwned + 'static,
    P: Providers,
    C: MessageCodec + Clone,
{
    if endpoints.is_empty() {
        return Err(FanOutError::Empty);
    }

    let mut pending: FuturesUnordered<_> = endpoints
        .iter()
        .map(|ep| ep.get_reply(transport, request.clone()))
        .collect();

    let mut errors = Vec::new();
    while let Some(result) = pending.next().await {
        match result {
            Ok(resp) => {
                assert_sometimes!(true, "fan_out_race_winner_found");
                return Ok(resp);
            }
            Err(err) => errors.push(err),
        }
    }
    assert_sometimes!(true, "fan_out_race_all_peers_failed");
    Err(FanOutError::AllFailed { errors })
}

/// Issue `request` to every endpoint, wait for **all** to complete, and
/// return the per-peer outcomes in input order.
///
/// Unlike [`fan_out_all`], this never short-circuits — every peer is given a
/// chance to reply, even after one or more failures. The returned vector has
/// exactly `endpoints.len()` entries, in the same order as `endpoints`, so
/// the caller can pair successes and failures with their senders by index.
///
/// Useful for diagnostics, gossip-style status checks, and any case where
/// per-peer outcome matters more than aggregate success/failure.
///
/// # Errors
///
/// Returns [`FanOutError::Empty`] if `endpoints` is empty. Otherwise the
/// function never errors at the fan-out level — peer-level errors live
/// inside the returned `Vec<Result<...>>`.
#[instrument(skip_all, fields(n = endpoints.len()))]
pub async fn fan_out_all_partial<Req, Resp, P, C>(
    transport: &NetTransport<P>,
    endpoints: &[ServiceEndpoint<Req, Resp, C>],
    request: Req,
) -> Result<Vec<Result<Resp, RpcError>>, FanOutError>
where
    Req: Serialize + Clone,
    Resp: DeserializeOwned + 'static,
    P: Providers,
    C: MessageCodec + Clone,
{
    if endpoints.is_empty() {
        return Err(FanOutError::Empty);
    }

    let futures: Vec<_> = endpoints
        .iter()
        .map(|ep| ep.get_reply(transport, request.clone()))
        .collect();

    let results = join_all(futures).await;
    if results.iter().any(|r| r.is_err()) {
        assert_sometimes!(true, "fan_out_partial_some_peer_failed");
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::rc::Rc;

    use super::*;
    use crate::rpc::ReplyError;
    use crate::rpc::test_support::{Echo, dispatch_reply, make_transport, register_servers};
    use crate::{Endpoint, JsonCodec, NetworkAddress, UID};

    #[tokio::test]
    async fn fan_out_all_returns_empty_error_for_no_endpoints() {
        let transport = make_transport();
        let endpoints: Vec<ServiceEndpoint<Echo, Echo, JsonCodec>> = Vec::new();
        let result = fan_out_all(&transport, &endpoints, Echo(1)).await;
        assert!(matches!(result, Err(FanOutError::Empty)));
    }

    #[test]
    fn fan_out_all_succeeds_when_every_peer_replies() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("build runtime");

        rt.block_on(async {
            let transport = Rc::new(make_transport());
            let (queues, endpoints) = register_servers(&transport, &[1, 2, 3]);

            let t = Rc::clone(&transport);
            let handle =
                tokio::task::spawn_local(
                    async move { fan_out_all(&t, &endpoints, Echo(42)).await },
                );

            tokio::task::yield_now().await;

            for (i, q) in queues.iter().enumerate() {
                let envelope = q.try_recv().expect("server should have received request");
                assert_eq!(envelope.request, Echo(42));
                dispatch_reply(&transport, &envelope, Ok(Echo(100 + i as u32)));
            }

            let result = handle.await.expect("join task").expect("fan_out_all");
            assert_eq!(result, vec![Echo(100), Echo(101), Echo(102)]);
        });
    }

    #[test]
    fn fan_out_all_aborts_on_first_failure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("build runtime");

        rt.block_on(async {
            let transport = Rc::new(make_transport());
            let (queues, endpoints) = register_servers(&transport, &[10, 11, 12]);

            let t = Rc::clone(&transport);
            let handle =
                tokio::task::spawn_local(async move { fan_out_all(&t, &endpoints, Echo(7)).await });

            tokio::task::yield_now().await;

            // Drain all three queues; only the second peer dispatches a reply
            // (a BrokenPromise error). The others stay un-replied — their
            // futures will be dropped when try_join_all aborts.
            for (i, q) in queues.iter().enumerate() {
                let envelope = q.try_recv().expect("server should have received request");
                if i == 1 {
                    dispatch_reply(&transport, &envelope, Err(ReplyError::BrokenPromise));
                }
            }

            let result = handle.await.expect("join task");
            match result {
                Err(FanOutError::AllFailed { errors }) => {
                    assert_eq!(errors.len(), 1);
                    assert!(matches!(
                        errors[0],
                        RpcError::Reply(ReplyError::BrokenPromise)
                    ));
                }
                other => panic!("expected AllFailed, got {other:?}"),
            }
        });
    }

    // ---- fan_out_quorum tests ----

    #[tokio::test]
    async fn fan_out_quorum_returns_empty_for_no_endpoints() {
        let transport = make_transport();
        let endpoints: Vec<ServiceEndpoint<Echo, Echo, JsonCodec>> = Vec::new();
        let result = fan_out_quorum(&transport, &endpoints, Echo(0), 1).await;
        assert!(matches!(result, Err(FanOutError::Empty)));
    }

    #[tokio::test]
    async fn fan_out_quorum_required_greater_than_n_fails_immediately() {
        let transport = make_transport();
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500);
        let ep = Endpoint::new(addr, UID::new(1, 1));
        let endpoints = vec![ServiceEndpoint::<Echo, Echo, JsonCodec>::new(ep, JsonCodec)];

        let result = fan_out_quorum(&transport, &endpoints, Echo(0), 5).await;
        match result {
            Err(FanOutError::QuorumNotMet {
                required,
                received,
                errors,
            }) => {
                assert_eq!(required, 5);
                assert_eq!(received, 0);
                assert!(errors.is_empty());
            }
            other => panic!("expected QuorumNotMet, got {other:?}"),
        }
    }

    #[test]
    fn fan_out_quorum_succeeds_with_two_of_three() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("build runtime");

        rt.block_on(async {
            let transport = Rc::new(make_transport());
            let (queues, endpoints) = register_servers(&transport, &[20, 21, 22]);

            let t = Rc::clone(&transport);
            let handle = tokio::task::spawn_local(async move {
                fan_out_quorum(&t, &endpoints, Echo(7), 2).await
            });

            tokio::task::yield_now().await;

            // Server 0 fails, servers 1 and 2 succeed → quorum of 2 met.
            for (i, q) in queues.iter().enumerate() {
                let envelope = q.try_recv().expect("envelope");
                let reply: Result<Echo, ReplyError> = if i == 0 {
                    Err(ReplyError::BrokenPromise)
                } else {
                    Ok(Echo(200 + i as u32))
                };
                dispatch_reply(&transport, &envelope, reply);
            }

            let replies = handle.await.expect("join task").expect("quorum met");
            assert_eq!(replies.len(), 2);
            // Replies are in completion order; both should be the success values.
            for r in &replies {
                assert!(*r == Echo(201) || *r == Echo(202), "unexpected reply {r:?}");
            }
        });
    }

    #[test]
    fn fan_out_quorum_returns_quorum_not_met_when_two_of_three_fail() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("build runtime");

        rt.block_on(async {
            let transport = Rc::new(make_transport());
            let (queues, endpoints) = register_servers(&transport, &[30, 31, 32]);

            let t = Rc::clone(&transport);
            let handle = tokio::task::spawn_local(async move {
                fan_out_quorum(&t, &endpoints, Echo(8), 2).await
            });

            tokio::task::yield_now().await;

            // Two failures → max_tolerable_failures = 1, so 2 failures aborts.
            for (i, q) in queues.iter().enumerate() {
                let envelope = q.try_recv().expect("envelope");
                let reply: Result<Echo, ReplyError> = if i < 2 {
                    Err(ReplyError::BrokenPromise)
                } else {
                    Ok(Echo(99))
                };
                dispatch_reply(&transport, &envelope, reply);
            }

            let result = handle.await.expect("join task");
            match result {
                Err(FanOutError::QuorumNotMet {
                    required,
                    received,
                    errors,
                }) => {
                    assert_eq!(required, 2);
                    assert!(received <= 1, "got {received} successes before abort");
                    assert!(errors.len() >= 2);
                }
                other => panic!("expected QuorumNotMet, got {other:?}"),
            }
        });
    }

    // ---- fan_out_race tests ----

    #[tokio::test]
    async fn fan_out_race_returns_empty_for_no_endpoints() {
        let transport = make_transport();
        let endpoints: Vec<ServiceEndpoint<Echo, Echo, JsonCodec>> = Vec::new();
        let result = fan_out_race(&transport, &endpoints, Echo(0)).await;
        assert!(matches!(result, Err(FanOutError::Empty)));
    }

    #[test]
    fn fan_out_race_returns_first_success() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("build runtime");

        rt.block_on(async {
            let transport = Rc::new(make_transport());
            let (queues, endpoints) = register_servers(&transport, &[40, 41, 42]);

            let t = Rc::clone(&transport);
            let handle =
                tokio::task::spawn_local(
                    async move { fan_out_race(&t, &endpoints, Echo(1)).await },
                );

            tokio::task::yield_now().await;

            // Only the middle peer dispatches a successful reply; the other
            // two stay un-replied. fan_out_race should still return Ok with
            // that single success and drop the rest.
            let envelope = queues[1].try_recv().expect("middle peer envelope");
            dispatch_reply(&transport, &envelope, Ok(Echo(555)));

            let resp = handle.await.expect("join task").expect("race success");
            assert_eq!(resp, Echo(555));
        });
    }

    #[test]
    fn fan_out_race_returns_all_failed_when_every_peer_errors() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("build runtime");

        rt.block_on(async {
            let transport = Rc::new(make_transport());
            let (queues, endpoints) = register_servers(&transport, &[50, 51, 52]);

            let t = Rc::clone(&transport);
            let handle =
                tokio::task::spawn_local(
                    async move { fan_out_race(&t, &endpoints, Echo(2)).await },
                );

            tokio::task::yield_now().await;

            for q in &queues {
                let envelope = q.try_recv().expect("envelope");
                dispatch_reply(&transport, &envelope, Err(ReplyError::BrokenPromise));
            }

            let result = handle.await.expect("join task");
            match result {
                Err(FanOutError::AllFailed { errors }) => {
                    assert_eq!(errors.len(), 3);
                }
                other => panic!("expected AllFailed, got {other:?}"),
            }
        });
    }

    // ---- fan_out_all_partial tests ----

    #[tokio::test]
    async fn fan_out_all_partial_returns_empty_for_no_endpoints() {
        let transport = make_transport();
        let endpoints: Vec<ServiceEndpoint<Echo, Echo, JsonCodec>> = Vec::new();
        let result = fan_out_all_partial(&transport, &endpoints, Echo(0)).await;
        assert!(matches!(result, Err(FanOutError::Empty)));
    }

    #[test]
    fn fan_out_all_partial_collects_per_peer_results() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("build runtime");

        rt.block_on(async {
            let transport = Rc::new(make_transport());
            let (queues, endpoints) = register_servers(&transport, &[60, 61, 62]);

            let t = Rc::clone(&transport);
            let handle = tokio::task::spawn_local(async move {
                fan_out_all_partial(&t, &endpoints, Echo(3)).await
            });

            tokio::task::yield_now().await;

            // Mixed outcomes: peer 0 fails, peers 1+2 succeed.
            for (i, q) in queues.iter().enumerate() {
                let envelope = q.try_recv().expect("envelope");
                let reply: Result<Echo, ReplyError> = if i == 0 {
                    Err(ReplyError::BrokenPromise)
                } else {
                    Ok(Echo(700 + i as u32))
                };
                dispatch_reply(&transport, &envelope, reply);
            }

            let results = handle.await.expect("join task").expect("partial vec");
            assert_eq!(results.len(), 3);
            assert!(results[0].is_err());
            assert_eq!(results[1].as_ref().unwrap(), &Echo(701));
            assert_eq!(results[2].as_ref().unwrap(), &Echo(702));
        });
    }
}
