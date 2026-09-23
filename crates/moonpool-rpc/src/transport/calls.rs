//! Starting, completing, abandoning and retransmitting calls.

use std::sync::{Arc, Weak};

use futures::channel::oneshot;
use moonpool_core::Providers;

use super::connection::{CloseReason, Connection, QueueRefusal};
use super::{
    Admission, Delivery, Origin, PendingCall, ReplyBytes, Shared, State, permanent_failure,
    permanent_reason,
};
use crate::call::client::{CallGuard, CallOwner};
use crate::call::reply::{LocalSink, ReplyRoute};
use crate::endpoint::Endpoint;
use crate::error::{CallIdentity, ErrorReason, Execution, RpcError};
use crate::protocol::{
    HEADER_LEN, REQUEST_FLAG_ONE_WAY, WireMessage, WireOutcome, encode_frame, encode_message,
    request_envelope_len,
};
use crate::stats::Counters;

/// A started call: its single completion and the guard that releases it.
pub(crate) type Started = (oneshot::Receiver<Result<ReplyBytes, RpcError>>, CallGuard);

fn request_frame(
    call_id: u64,
    endpoint: &Endpoint,
    identity: CallIdentity,
    flags: u8,
    body: Vec<u8>,
    max_frame_bytes: u32,
) -> Result<Vec<u8>, RpcError> {
    let payload = encode_message(&WireMessage::Request {
        call_id,
        incarnation: endpoint.incarnation(),
        token: endpoint.token(),
        interface: identity.interface.0,
        interface_version: identity.interface.1,
        method: identity.method,
        schema: identity.schema,
        codec: identity.codec,
        flags,
        metadata: Vec::new(),
        body,
    });
    encode_frame(&payload, max_frame_bytes).map_err(|_| {
        RpcError::not_admitted(ErrorReason::FrameTooLarge {
            size: payload.len() as u64,
            limit: max_frame_bytes,
        })
    })
}

fn overloaded() -> RpcError {
    RpcError::not_admitted(ErrorReason::Overloaded)
}

impl<P: Providers> Shared<P> {
    /// The checks every outgoing request passes before anything is queued:
    /// the frame limit and the failure monitor's permanent verdicts.
    fn precheck(
        &self,
        state: &State,
        endpoint: &Endpoint,
        body_len: usize,
    ) -> Result<(), RpcError> {
        // Same frame limit on both routes: nothing oversized is admitted.
        let size = request_envelope_len(body_len) as u64;
        if size > u64::from(self.config.max_frame_bytes) {
            return Err(RpcError::not_admitted(ErrorReason::FrameTooLarge {
                size,
                limit: self.config.max_frame_bytes,
            }));
        }
        // A dynamic endpoint the server already declared gone fails at once,
        // without a round trip; this attempt never left.
        if let Some(failure) = state.monitor.permanent_failure(endpoint) {
            Counters::bump(&self.counters.calls_failed_fast);
            return Err(RpcError::not_admitted(permanent_reason(failure)));
        }
        Ok(())
    }

    /// Start one call. The returned receiver completes exactly once.
    pub(crate) fn start_call(
        &self,
        endpoint: &Endpoint,
        identity: CallIdentity,
        body: Vec<u8>,
        delivery: Delivery,
    ) -> Result<Started, RpcError> {
        let local = self.address == Some(endpoint.address());
        let (sender, receiver) = oneshot::channel();
        let mut state = self.lock();
        self.precheck(&state, endpoint, body.len())?;
        if state.pending.len() >= self.config.max_pending_calls {
            return Err(overloaded());
        }
        let call_id = state.next_call_id;
        state.next_call_id = call_id.checked_add(1).ok_or_else(overloaded)?;
        let owner: Weak<dyn CallOwner> = self.this.clone();
        let guard = CallGuard::new(owner, call_id);

        if local {
            // Admission below is synchronous: once it returns, the request
            // either reached a receiver or was refused with an outcome. A
            // local call cannot lose its connection, so it is never
            // retransmitted.
            state.pending.insert(
                call_id,
                PendingCall {
                    origin: Origin::Local,
                    endpoint: *endpoint,
                    identity,
                    transmitted: true,
                    earlier_transmitted: false,
                    retained: None,
                    sender,
                },
            );
            drop(state);
            Counters::bump(&self.counters.calls_started);
            let sink: Weak<dyn LocalSink> = self.this.clone();
            // The same bytes, validation order, admission and frame limit as
            // the remote path; only the socket is skipped.
            self.admit(
                &Admission {
                    incarnation: endpoint.incarnation(),
                    token: endpoint.token(),
                    identity,
                    body: &body,
                },
                ReplyRoute::Local { sink, call_id },
                None,
            );
            return Ok((receiver, guard));
        }

        let retained = (delivery == Delivery::Reliable).then(|| body.clone());
        let frame = request_frame(
            call_id,
            endpoint,
            identity,
            0,
            body,
            self.config.max_frame_bytes,
        )?;
        let connection = self.peer_connection(&mut state, endpoint.address())?;
        match connection.push_request(frame, Some(call_id), self.now()) {
            Ok(()) => {}
            Err(QueueRefusal::Full) => return Err(overloaded()),
            Err(QueueRefusal::Closed) => {
                return Err(RpcError::not_admitted(ErrorReason::Disconnected));
            }
        }
        state.pending.insert(
            call_id,
            PendingCall {
                origin: Origin::Connection(connection.id()),
                endpoint: *endpoint,
                identity,
                transmitted: false,
                earlier_transmitted: false,
                retained,
                sender,
            },
        );
        drop(state);
        Counters::bump(&self.counters.calls_started);
        if delivery == Delivery::Reliable {
            Counters::bump(&self.counters.reliable_calls_started);
        }
        Ok((receiver, guard))
    }

    /// Queue one one-way request. Success means it was handed to a
    /// connection (or a local receiver's admission), nothing more.
    pub(crate) fn send_one_way(
        &self,
        endpoint: &Endpoint,
        identity: CallIdentity,
        body: Vec<u8>,
    ) -> Result<(), RpcError> {
        let local = self.address == Some(endpoint.address());
        let mut state = self.lock();
        self.precheck(&state, endpoint, body.len())?;
        if local {
            drop(state);
            Counters::bump(&self.counters.one_way_sent);
            self.admit(
                &Admission {
                    incarnation: endpoint.incarnation(),
                    token: endpoint.token(),
                    identity,
                    body: &body,
                },
                ReplyRoute::Discard,
                None,
            );
            return Ok(());
        }
        let frame = request_frame(
            0,
            endpoint,
            identity,
            REQUEST_FLAG_ONE_WAY,
            body,
            self.config.max_frame_bytes,
        )?;
        let connection = self.peer_connection(&mut state, endpoint.address())?;
        let queued = connection.push_request(frame, None, self.now());
        drop(state);
        match queued {
            Ok(()) => {
                Counters::bump(&self.counters.one_way_sent);
                Ok(())
            }
            Err(QueueRefusal::Full) => Err(overloaded()),
            Err(QueueRefusal::Closed) => Err(RpcError::not_admitted(ErrorReason::Disconnected)),
        }
    }

    /// Decide whether a request frame may leave on `connection`: it must fit
    /// the peer's announced frame limit. A frame that does not is refused
    /// for its own call (never sent, so `NotAdmitted`) and the session
    /// stays up; a one-way frame that does not is dropped and counted.
    pub(super) fn admit_transmit(
        &self,
        connection: &Connection,
        call_id: Option<u64>,
        frame_len: usize,
    ) -> bool {
        let limit = connection
            .peer_hello()
            .map_or(self.config.max_frame_bytes, |hello| hello.max_frame_bytes);
        let size = frame_len.saturating_sub(HEADER_LEN) as u64;
        let mut state = self.lock();
        if size > u64::from(limit) {
            let call = call_id.and_then(|call_id| state.pending.remove(&call_id));
            drop(state);
            tracing::debug!(
                ?call_id,
                size,
                limit,
                "rpc request exceeds the peer's frame limit"
            );
            match call {
                Some(call) => {
                    let error = ErrorReason::FrameTooLarge { size, limit };
                    let error = if call.earlier_transmitted {
                        RpcError::new(error, Execution::MaybeExecuted)
                    } else {
                        RpcError::not_admitted(error)
                    };
                    let _ = call.sender.send(Err(error));
                }
                None if call_id.is_none() => Counters::bump(&self.counters.one_way_dropped),
                None => {}
            }
            return false;
        }
        if let Some(call) = call_id.and_then(|call_id| state.pending.get_mut(&call_id)) {
            call.transmitted = true;
        }
        true
    }

    /// Complete a pending call from `origin`.
    pub(super) fn complete(&self, origin: Origin, call_id: u64, outcome: WireOutcome) {
        let mut state = self.lock();
        let Some(call) = state.pending.get(&call_id) else {
            drop(state);
            Counters::bump(&self.counters.late_replies);
            tracing::debug!(call_id, "rpc late reply discarded");
            return;
        };
        if call.origin != origin {
            drop(state);
            Counters::bump(&self.counters.misrouted_replies);
            tracing::warn!(
                call_id,
                ?origin,
                "rpc reply arrived on the wrong session, discarded"
            );
            return;
        }
        let Some(call) = state.pending.remove(&call_id) else {
            return;
        };
        let mut released = Vec::new();
        let mut learned = false;
        if let WireOutcome::Err(error) = &outcome
            && let Some(failure) = permanent_failure(*error)
            && state.monitor.endpoint_failed(&call.endpoint, failure)
        {
            learned = true;
            released = self.release_failed(&mut state);
        }
        drop(state);
        if learned {
            self.watch.notify();
        }
        let result = match outcome {
            WireOutcome::Ok { codec, body } => Ok((codec, body)),
            WireOutcome::Err(error) => {
                let error = RpcError::from_wire(error, call.identity);
                // A rejection proves non-admission for this attempt only.
                Err(if call.earlier_transmitted {
                    error.at_least(Execution::MaybeExecuted)
                } else {
                    error
                })
            }
        };
        let _ = call.sender.send(result);
        for (call, error) in released {
            let _ = call.sender.send(Err(error));
        }
    }

    /// Remove every retained (reliable) call whose endpoint the monitor now
    /// knows failed for good, withdrawing its queued frame, and return it
    /// with its terminal error: retention ends promptly, never waits.
    fn release_failed(&self, state: &mut State) -> Vec<(PendingCall, RpcError)> {
        let doomed: Vec<(u64, ErrorReason)> = state
            .pending
            .iter()
            .filter(|(_, call)| call.retained.is_some())
            .filter_map(|(call_id, call)| {
                state
                    .monitor
                    .permanent_failure(&call.endpoint)
                    .map(|failure| (*call_id, permanent_reason(failure)))
            })
            .collect();
        doomed
            .into_iter()
            .filter_map(|(call_id, reason)| {
                let mut call = state.pending.remove(&call_id)?;
                if let Origin::Connection(id) = call.origin
                    && let Some(connection) = state.connections.get(&id)
                    && !call.transmitted
                {
                    call.transmitted = !connection.retract(call_id);
                }
                let execution = if call.may_have_executed() {
                    Execution::MaybeExecuted
                } else {
                    Execution::NotAdmitted
                };
                Counters::bump(&self.counters.retention_released_by_failure);
                Some((call, RpcError::new(reason, execution)))
            })
            .collect()
    }

    /// Whether a pending call's current or an earlier attempt began to
    /// leave; `None` once it completed.
    pub(crate) fn call_transmitted(&self, call_id: u64) -> Option<bool> {
        self.lock()
            .pending
            .get(&call_id)
            .map(PendingCall::may_have_executed)
    }

    /// Tear down a finished connection: settle its peer, report the
    /// disconnect, fail its at-most-once calls and move its reliable calls
    /// to the next connection.
    pub(super) fn close_connection(&self, connection: &Arc<Connection>, returned: CloseReason) {
        let established = connection.is_established();
        let reason = connection.close(returned);
        let now = self.now();
        let mut state = self.lock();
        state.connections.remove(&connection.id());
        let address = connection.peer_address();
        let mut notify = false;
        if let Some(address) = address {
            if let Some(peer) = state.peers.get_mut(&address)
                && peer
                    .current
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, connection))
            {
                peer.current = None;
                peer.session_ended(now, &self.config.peer);
            }
            // Every end of a selected connection is a disconnect event; only
            // failures extend the address's failure run.
            state.monitor.disconnected(
                address,
                reason.is_failure(),
                now,
                self.config.peer.failure_detection_delay,
            );
            notify = true;
        }
        let origin = Origin::Connection(connection.id());
        let carried: Vec<u64> = state
            .pending
            .iter()
            .filter(|(_, call)| call.origin == origin)
            .map(|(call_id, _)| *call_id)
            .collect();
        let mut failed = Vec::new();
        let mut resend = Vec::new();
        for call_id in carried {
            let Some(call) = state.pending.remove(&call_id) else {
                continue;
            };
            if call.retained.is_some() && address.is_some() {
                resend.push((call_id, call));
            } else {
                let error = disconnect_error(&reason, established, &call);
                if call.transmitted && matches!(reason, CloseReason::Checksum(_)) {
                    Counters::bump(&self.counters.calls_failed_by_corruption);
                }
                failed.push((call, error));
            }
        }
        if let Some(address) = address {
            let probe = reason.is_failure() && state.monitor.references(address) > 0;
            if !resend.is_empty() || probe {
                failed.extend(self.retransmit(&mut state, address, resend));
            }
        }
        drop(state);
        self.log_close(connection, &reason, established);
        if notify {
            self.watch.notify();
        }
        for (call, error) in failed {
            let _ = call.sender.send(Err(error));
        }
    }

    /// Queue `calls` again on the peer's next connection (opening one even
    /// when `calls` is empty: a probe for a watched, failing address).
    /// Returns the calls that could not be requeued, with their errors.
    fn retransmit(
        &self,
        state: &mut State,
        address: std::net::SocketAddr,
        calls: Vec<(u64, PendingCall)>,
    ) -> Vec<(PendingCall, RpcError)> {
        let mut failed = Vec::new();
        let connection = match self.peer_connection(state, address) {
            Ok(connection) => connection,
            Err(error) => {
                for (_, call) in calls {
                    let error = RpcError::new(
                        error.reason().clone(),
                        if call.may_have_executed() {
                            Execution::MaybeExecuted
                        } else {
                            Execution::NotAdmitted
                        },
                    );
                    failed.push((call, error));
                }
                return failed;
            }
        };
        let now = self.now();
        for (call_id, mut call) in calls {
            call.earlier_transmitted |= call.transmitted;
            call.transmitted = false;
            if let Some(failure) = state.monitor.permanent_failure(&call.endpoint) {
                let execution = if call.earlier_transmitted {
                    Execution::MaybeExecuted
                } else {
                    Execution::NotAdmitted
                };
                failed.push((call, RpcError::new(permanent_reason(failure), execution)));
                continue;
            }
            let body = call.retained.clone().unwrap_or_default();
            let frame = request_frame(
                call_id,
                &call.endpoint,
                call.identity,
                0,
                body,
                self.config.max_frame_bytes,
            );
            // A retained request was admitted once already: it is queued
            // regardless of the connection's request cap (the pending-call
            // budget bounds it), never refused as overloaded on the way.
            let queued = frame.and_then(|frame| {
                if connection.adopt_requests(vec![(frame, Some(call_id))], now) {
                    Ok(())
                } else {
                    Err(RpcError::not_admitted(ErrorReason::Disconnected))
                }
            });
            match queued {
                Ok(()) => {
                    Counters::bump(&self.counters.retransmissions);
                    call.origin = Origin::Connection(connection.id());
                    state.pending.insert(call_id, call);
                }
                Err(error) => {
                    let execution = if call.earlier_transmitted {
                        Execution::MaybeExecuted
                    } else {
                        Execution::NotAdmitted
                    };
                    failed.push((call, RpcError::new(error.reason().clone(), execution)));
                }
            }
        }
        failed
    }

    fn log_close(&self, connection: &Connection, reason: &CloseReason, established: bool) {
        match reason {
            CloseReason::Protocol(detail)
            | CloseReason::Checksum(detail)
            | CloseReason::Version(detail) => {
                Counters::bump(&self.counters.protocol_violations);
                if matches!(reason, CloseReason::Checksum(_)) {
                    Counters::bump(&self.counters.checksum_failures);
                    if established {
                        Counters::bump(&self.counters.established_checksum_failures);
                    }
                }
                if matches!(reason, CloseReason::Version(_)) {
                    Counters::bump(&self.counters.version_rejections);
                }
                tracing::warn!(peer = %connection.peer(), %detail, "rpc protocol violation, connection closed");
            }
            CloseReason::PingTimeout => {
                Counters::bump(&self.counters.ping_timeouts);
                tracing::debug!(peer = %connection.peer(), "rpc connection failed its ping");
            }
            CloseReason::Idle => {
                Counters::bump(&self.counters.idle_closes);
                tracing::debug!(peer = %connection.peer(), "rpc idle connection closed");
            }
            CloseReason::Replaced => {
                Counters::bump(&self.counters.replaced_connections);
                tracing::debug!(peer = %connection.peer(), ?reason, "rpc duplicate connection closed");
            }
            _ => tracing::debug!(peer = %connection.peer(), ?reason, "rpc connection closed"),
        }
    }
}

/// What a connection's end proves about an at-most-once call it carried.
fn disconnect_error(reason: &CloseReason, established: bool, call: &PendingCall) -> RpcError {
    if let CloseReason::ConnectFailed(detail) = reason {
        RpcError::not_admitted(ErrorReason::ConnectFailed(detail.clone()))
    } else if !established {
        // Requests wait for the handshake: none of these left.
        RpcError::not_admitted(ErrorReason::ConnectFailed(format!(
            "session closed during handshake: {reason:?}"
        )))
    } else if call.transmitted {
        RpcError::new(ErrorReason::Disconnected, Execution::MaybeExecuted)
    } else {
        RpcError::not_admitted(ErrorReason::Disconnected)
    }
}

impl<P: Providers> CallOwner for Shared<P> {
    fn abandon(&self, call_id: u64) -> Option<bool> {
        let mut state = self.lock();
        let call = state.pending.remove(&call_id)?;
        // A request still queued behind the handshake or other frames is
        // withdrawn, so "not transmitted" stays true after the caller leaves.
        // If the writer already took it, it may be on the wire. Retention
        // (a reliable call's body) goes with the call.
        let transmitted = call.earlier_transmitted
            || call.transmitted
            || match call.origin {
                Origin::Local => true,
                Origin::Connection(id) => state
                    .connections
                    .get(&id)
                    .is_none_or(|connection| !connection.retract(call_id)),
            };
        drop(state);
        Counters::bump(&self.counters.calls_abandoned);
        tracing::debug!(call_id, transmitted, "rpc call abandoned by its caller");
        Some(transmitted)
    }

    fn transmitted(&self, call_id: u64) -> Option<bool> {
        self.call_transmitted(call_id)
    }
}

impl<P: Providers> LocalSink for Shared<P> {
    fn complete_local(&self, call_id: u64, outcome: WireOutcome) {
        self.complete(Origin::Local, call_id, outcome);
    }
}
