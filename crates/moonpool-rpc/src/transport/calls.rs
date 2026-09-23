//! Starting, completing, abandoning and retransmitting calls.

use std::sync::{Arc, Weak};

use futures::channel::oneshot;
use moonpool_core::Providers;

use super::connection::{CloseReason, Connection, QueueRefusal};
use super::{
    Admission, Completion, Delivery, Origin, PendingCall, ReplyBytes, Shared, State,
    permanent_failure, permanent_reason,
};
use crate::call::client::{CallGuard, CallOwner};
use crate::call::reply::{LocalSink, Outstanding, ReplyRoute};
use crate::endpoint::Endpoint;
use crate::error::{CallIdentity, ErrorReason, Execution, RpcError};
use crate::protocol::{
    HEADER_LEN, REQUEST_FLAG_ONE_WAY, WireMessage, WireOutcome, encode_frame, encode_message,
    request_envelope_len,
};
use crate::security::CredentialSource;
use crate::stats::Counters;
use crate::stream::consumer::AckRoute;
use crate::stream::producer::Terminal;
use crate::transport::upgrade::PeerContext;

/// Where a call's credentials come from, if anywhere.
pub(super) type Source = Option<Arc<dyn CredentialSource>>;

/// A started call: its single completion and the guard that releases it.
pub(crate) type Started = (oneshot::Receiver<Result<ReplyBytes, RpcError>>, CallGuard);

/// The flags and stream window of one request frame.
#[derive(Debug, Clone, Copy)]
pub(super) struct RequestKind {
    pub(super) flags: u8,
    pub(super) stream_window: u64,
}

impl RequestKind {
    pub(super) const TWO_WAY: Self = Self {
        flags: 0,
        stream_window: 0,
    };
    pub(super) const ONE_WAY: Self = Self {
        flags: REQUEST_FLAG_ONE_WAY,
        stream_window: 0,
    };
}

pub(super) fn request_frame(
    call_id: u64,
    endpoint: &Endpoint,
    identity: CallIdentity,
    kind: RequestKind,
    metadata: Vec<u8>,
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
        flags: kind.flags,
        stream_window: kind.stream_window,
        metadata,
        body,
    });
    encode_frame(&payload, max_frame_bytes).map_err(|_| {
        RpcError::not_admitted(ErrorReason::FrameTooLarge {
            size: payload.len() as u64,
            limit: max_frame_bytes,
        })
    })
}

/// The credential section of one attempt to `endpoint`, asked from
/// `source` now. Empty without a source or a credential.
pub(super) fn credential_section(
    source: Option<&Arc<dyn CredentialSource>>,
    endpoint: &Endpoint,
) -> Result<Vec<u8>, RpcError> {
    let Some(credential) = source.and_then(|source| source.credential(endpoint)) else {
        return Ok(Vec::new());
    };
    let section =
        crate::protocol::metadata::encode_bearer(credential.expose()).ok_or_else(|| {
            RpcError::not_admitted(ErrorReason::Encode(format!(
                "a {}-byte credential does not fit the metadata section",
                credential.len()
            )))
        })?;
    Ok(section)
}

pub(super) fn overloaded() -> RpcError {
    RpcError::not_admitted(ErrorReason::Overloaded)
}

impl<P: Providers> Shared<P> {
    /// Why a request carrying a credential must not be written on
    /// `connection`, if it must not: the credential then never leaves this
    /// process. Checked when the frame is about to be written, the first
    /// moment the session is known.
    fn credential_refusal(&self, connection: &Connection) -> Option<String> {
        let context = connection.peer_context();
        if connection
            .peer_hello()
            .is_some_and(|hello| hello.version < crate::protocol::CREDENTIALS_VERSION)
        {
            return Some("the session runs protocol version 1, which carries no credential".into());
        }
        if connection.direction() == super::connection::Direction::Inbound
            && context
                .as_ref()
                .is_none_or(|context| context.identity().is_none())
        {
            return Some("an accepted session never authenticated the peer that dialed it".into());
        }
        let encrypted = context.as_ref().is_some_and(PeerContext::is_encrypted);
        if !encrypted && !self.config.security.sends_credentials_over_plaintext() {
            return Some("the session is not encrypted".into());
        }
        None
    }

    /// The checks every outgoing request passes before anything is queued:
    /// the frame limit and the failure monitor's permanent verdicts.
    pub(super) fn precheck(
        &self,
        state: &State,
        endpoint: &Endpoint,
        envelope_len: usize,
    ) -> Result<(), RpcError> {
        // Same frame limit on both routes: nothing oversized is admitted.
        let size = envelope_len as u64;
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

    /// The credential source a call uses: the client's own, else the
    /// runtime's.
    pub(super) fn credential_source(
        &self,
        client: Option<Arc<dyn CredentialSource>>,
    ) -> Option<Arc<dyn CredentialSource>> {
        client.or_else(|| self.config.security.credentials().cloned())
    }

    /// What every new call checks and asks first: the runtime is not
    /// shutting down; then the call's credential source and its first
    /// attempt's credential section.
    pub(super) fn call_credentials(
        &self,
        client: Option<Arc<dyn CredentialSource>>,
        endpoint: &Endpoint,
    ) -> Result<(Source, Vec<u8>), RpcError> {
        self.refuse_if_closing()?;
        let source = self.credential_source(client);
        // Asked outside the state lock: the source is application code.
        let section = credential_section(source.as_ref(), endpoint)?;
        Ok((source, section))
    }

    /// Refuse a reliable call whose body would exceed the retained-bytes
    /// budget.
    fn check_retention(&self, state: &State, body_len: usize) -> Result<(), RpcError> {
        let retained: u64 = state
            .pending
            .values()
            .filter_map(|call| call.retained.as_ref())
            .map(|body| body.len() as u64)
            .sum();
        if retained.saturating_add(body_len as u64) > self.config.limits.max_retained_bytes {
            Counters::bump(&self.counters.overload_refusals);
            return Err(overloaded());
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
        credentials: Option<Arc<dyn CredentialSource>>,
    ) -> Result<Started, RpcError> {
        let (credentials, metadata) = self.call_credentials(credentials, endpoint)?;
        let local = self.address == Some(endpoint.address());
        let (sender, receiver) = oneshot::channel();
        let mut state = self.lock();
        self.precheck(
            &state,
            endpoint,
            request_envelope_len(body.len()) + metadata.len(),
        )?;
        if state.pending.len() >= self.config.max_pending_calls {
            return Err(overloaded());
        }
        if delivery == Delivery::Reliable {
            self.check_retention(&state, body.len())?;
        }
        let call_id = state.next_call_id;
        state.next_call_id = call_id.checked_add(1).ok_or_else(overloaded)?;
        // The call's guard is created only once the call is registered and
        // the state lock released: a guard dropped by an early error return
        // would abandon the call by locking the state it is still holding.
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
                    credentials: None,
                    completion: Completion::Reply(sender),
                },
            );
            drop(state);
            let guard = self.call_guard(call_id);
            Counters::bump(&self.counters.calls_started);
            let sink: Weak<dyn LocalSink> = self.this.clone();
            // The same bytes, validation order, security check, admission
            // and frame limit as the remote path; only the socket is
            // skipped.
            let context = self.context(
                ReplyRoute::Local { sink, call_id },
                None,
                Some(Outstanding::local(&self.counters)),
            );
            self.admit(
                &Admission {
                    local: true,
                    incarnation: endpoint.incarnation(),
                    token: endpoint.token(),
                    identity,
                    stream_window: None,
                    metadata: Some(&metadata),
                    body: &body,
                },
                context,
            );
            return Ok((receiver, guard));
        }

        let retained = (delivery == Delivery::Reliable).then(|| body.clone());
        let frame = request_frame(
            call_id,
            endpoint,
            identity,
            RequestKind::TWO_WAY,
            metadata,
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
                credentials: credentials.filter(|_| delivery == Delivery::Reliable),
                completion: Completion::Reply(sender),
            },
        );
        drop(state);
        let guard = self.call_guard(call_id);
        Counters::bump(&self.counters.calls_started);
        if delivery == Delivery::Reliable {
            Counters::bump(&self.counters.reliable_calls_started);
        }
        Ok((receiver, guard))
    }

    /// The guard that abandons pending call `call_id` if its caller stops
    /// waiting. Create it only after the state lock is released.
    pub(super) fn call_guard(&self, call_id: u64) -> CallGuard {
        let owner: Weak<dyn CallOwner> = self.this.clone();
        CallGuard::new(owner, call_id)
    }

    /// Queue one one-way request. Success means it was handed to a
    /// connection (or a local receiver's admission), nothing more.
    pub(crate) fn send_one_way(
        &self,
        endpoint: &Endpoint,
        identity: CallIdentity,
        body: Vec<u8>,
        credentials: Option<Arc<dyn CredentialSource>>,
    ) -> Result<(), RpcError> {
        let (_, metadata) = self.call_credentials(credentials, endpoint)?;
        let local = self.address == Some(endpoint.address());
        let mut state = self.lock();
        self.precheck(
            &state,
            endpoint,
            request_envelope_len(body.len()) + metadata.len(),
        )?;
        if local {
            drop(state);
            Counters::bump(&self.counters.one_way_sent);
            let context = self.context(ReplyRoute::Discard, None, None);
            self.admit(
                &Admission {
                    local: true,
                    incarnation: endpoint.incarnation(),
                    token: endpoint.token(),
                    identity,
                    stream_window: None,
                    metadata: Some(&metadata),
                    body: &body,
                },
                context,
            );
            return Ok(());
        }
        let frame = request_frame(
            0,
            endpoint,
            identity,
            RequestKind::ONE_WAY,
            metadata,
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
        frame: &[u8],
    ) -> bool {
        let limit = connection
            .peer_hello()
            .map_or(self.config.max_frame_bytes, |hello| hello.max_frame_bytes);
        let payload = frame.get(HEADER_LEN..).unwrap_or_default();
        let size = payload.len() as u64;
        let refusal = if size > u64::from(limit) {
            tracing::debug!(
                ?call_id,
                size,
                limit,
                "rpc request exceeds the peer's frame limit"
            );
            Some(ErrorReason::FrameTooLarge { size, limit })
        } else if crate::protocol::request_metadata_len(payload).is_some_and(|len| len > 0) {
            self.credential_refusal(connection)
                .map(ErrorReason::CredentialWithheld)
        } else {
            None
        };
        let mut state = self.lock();
        if let Some(refusal) = refusal {
            let call = call_id.and_then(|call_id| state.pending.remove(&call_id));
            drop(state);
            match call {
                Some(call) => {
                    let error = if call.earlier_transmitted {
                        RpcError::new(refusal, Execution::MaybeExecuted)
                    } else {
                        RpcError::not_admitted(refusal)
                    };
                    call.completion.finish(Err(error));
                }
                None if call_id.is_none() => Counters::bump(&self.counters.one_way_dropped),
                None => {}
            }
            return false;
        }
        if crate::protocol::request_metadata_len(payload).is_some_and(|len| len > 0) {
            Counters::bump(&self.counters.credentials_attached);
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
        call.completion.finish(result);
        for (call, error) in released {
            call.completion.finish(Err(error));
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
            let Some(call) = state.pending.get_mut(&call_id) else {
                continue;
            };
            if call.retained.is_some() && address.is_some() && call.stream().is_none() {
                // Stays pending (waiting on the ended connection) while its
                // next copy is built outside the lock.
                call.earlier_transmitted |= call.transmitted;
                call.transmitted = false;
                resend.push(Resend {
                    call_id,
                    endpoint: call.endpoint,
                    identity: call.identity,
                    body: call.retained.clone().unwrap_or_default(),
                    credentials: call.credentials.clone(),
                });
            } else if let Some(call) = state.pending.remove(&call_id) {
                let error = disconnect_error(&reason, established, &call);
                if call.transmitted && matches!(reason, CloseReason::Checksum(_)) {
                    Counters::bump(&self.counters.calls_failed_by_corruption);
                }
                failed.push((call, error));
            }
        }
        let probe = address
            .is_some_and(|address| reason.is_failure() && state.monitor.references(address) > 0);
        drop(state);
        self.log_close(connection, &reason, established);
        if notify {
            self.watch.notify();
        }
        if let Some(address) = address
            && (!resend.is_empty() || probe)
        {
            // Each copy asks its credential source again, outside every
            // lock: a token refreshed since the last copy is the one sent,
            // and the source may use the runtime.
            let copies: Vec<(u64, Result<Vec<u8>, RpcError>)> = resend
                .into_iter()
                .map(|copy| {
                    let frame = credential_section(copy.credentials.as_ref(), &copy.endpoint)
                        .and_then(|metadata| {
                            request_frame(
                                copy.call_id,
                                &copy.endpoint,
                                copy.identity,
                                RequestKind::TWO_WAY,
                                metadata,
                                copy.body,
                                self.config.max_frame_bytes,
                            )
                        });
                    (copy.call_id, frame)
                })
                .collect();
            let mut state = self.lock();
            failed.extend(self.retransmit(&mut state, address, origin, copies));
        }
        for (call, error) in failed {
            call.completion.finish(Err(error));
        }
        // Streams produced over this session end with it; never resumed.
        for stream in connection.take_streams() {
            stream.terminate(Terminal::Disconnected);
            Counters::bump(&self.counters.streams_disconnected);
        }
    }

    /// Queue the prepared `copies` of calls still waiting on the ended
    /// connection `ended` on the peer's next connection (opening one even
    /// when there are none: a probe for a watched, failing address). A call
    /// completed, abandoned or ended by a shutdown meanwhile is skipped.
    /// Returns the calls that could not be requeued, with their errors.
    fn retransmit(
        &self,
        state: &mut State,
        address: std::net::SocketAddr,
        ended: Origin,
        copies: Vec<(u64, Result<Vec<u8>, RpcError>)>,
    ) -> Vec<(PendingCall, RpcError)> {
        let execution = |call: &PendingCall| {
            if call.may_have_executed() {
                Execution::MaybeExecuted
            } else {
                Execution::NotAdmitted
            }
        };
        let mut failed = Vec::new();
        let connection = self.peer_connection(state, address);
        let now = self.now();
        for (call_id, frame) in copies {
            if state
                .pending
                .get(&call_id)
                .is_none_or(|call| call.origin != ended)
            {
                continue;
            }
            let Some(mut call) = state.pending.remove(&call_id) else {
                continue;
            };
            if let Some(failure) = state.monitor.permanent_failure(&call.endpoint) {
                let execution = execution(&call);
                failed.push((call, RpcError::new(permanent_reason(failure), execution)));
                continue;
            }
            // A retained request was admitted once already: it is queued
            // regardless of the connection's request cap (the pending-call
            // budget bounds it), never refused as overloaded on the way.
            let queued = match (&connection, &frame) {
                (Err(error), _) | (_, Err(error)) => Err(error.reason().clone()),
                (Ok(connection), Ok(frame)) => {
                    if connection.adopt_requests(vec![(frame.clone(), Some(call_id))], now) {
                        Ok(connection.id())
                    } else {
                        Err(ErrorReason::Disconnected)
                    }
                }
            };
            match queued {
                Ok(id) => {
                    Counters::bump(&self.counters.retransmissions);
                    call.origin = Origin::Connection(id);
                    state.pending.insert(call_id, call);
                }
                Err(reason) => {
                    let execution = execution(&call);
                    failed.push((call, RpcError::new(reason, execution)));
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

/// A retained call's next copy, prepared outside the state lock.
struct Resend {
    call_id: u64,
    endpoint: Endpoint,
    identity: CallIdentity,
    body: Vec<u8>,
    credentials: Option<Arc<dyn CredentialSource>>,
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
        // A stream whose request may have reached the server is cancelled
        // there: the producer stops and its unwritten items are dropped.
        let cancel = match (call.stream(), call.origin) {
            (Some(_), Origin::Connection(id)) if transmitted => state
                .connections
                .get(&id)
                .cloned()
                .map(|connection| AckRoute::Remote(Arc::downgrade(&connection))),
            (Some(stream), Origin::Local) => stream.route(),
            _ => None,
        };
        drop(state);
        match cancel {
            Some(AckRoute::Remote(connection)) => {
                if let Some(connection) = connection.upgrade() {
                    connection.signal_cancel(call_id);
                }
                Counters::bump(&self.counters.streams_abandoned);
            }
            Some(AckRoute::Local(producer)) => {
                if let Some(producer) = producer.upgrade()
                    && producer.cancel()
                {
                    Counters::bump(&self.counters.streams_cancelled);
                }
                Counters::bump(&self.counters.streams_abandoned);
            }
            None => {}
        }
        drop(call);
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
