//! Reply streams on the runtime: opening one as a caller, admitting one as
//! a server, and routing items, ends, acknowledgements and cancels between
//! the two cores (see [`crate::stream`]).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use moonpool_core::Providers;

use super::calls::{RequestKind, overloaded, request_frame};
use super::connection::{Connection, QueueRefusal};
use super::{
    Admission, Completion, Origin, PendingCall, Shared, StreamCompletion, permanent_failure,
};
use crate::call::client::{CallGuard, CallOwner};
use crate::call::reply::{LocalSink, Outstanding, ReplyContext, ReplyRoute};
use crate::codec::CodecId;
use crate::endpoint::Endpoint;
use crate::error::{CallIdentity, ErrorReason, RpcError};
use crate::protocol::{HEADER_LEN, REQUEST_FLAG_STREAM, WireError, stream_request_envelope_len};
use crate::stats::Counters;
use crate::stream::consumer::{AckRoute, ConsumerCore, Intook};
use crate::stream::producer::{AckEffect, CoreRoute, LocalStreamSink, StreamCore, StreamSlot};

/// A started stream: its consumer and the guard that abandons it.
pub(crate) type StartedStream = (Arc<ConsumerCore>, CallGuard);

impl<P: Providers> Shared<P> {
    /// Open a reply stream to `endpoint` announcing `window` bytes.
    pub(crate) fn start_stream(
        &self,
        endpoint: &Endpoint,
        identity: CallIdentity,
        body: Vec<u8>,
        window: u64,
    ) -> Result<StartedStream, RpcError> {
        let local = self.address == Some(endpoint.address());
        let mut state = self.lock();
        self.precheck(&state, endpoint, stream_request_envelope_len(body.len()))?;
        if state.pending.len() >= self.config.max_pending_calls {
            return Err(overloaded());
        }
        // The window is memory this runtime promises to hold: reserve it
        // from the buffer budget (released when the consumer is dropped).
        let before = self
            .counters
            .stream_window_reserved
            .fetch_add(window, Ordering::Relaxed);
        if before.saturating_add(window) > self.config.streams.max_buffered_bytes {
            self.counters
                .stream_window_reserved
                .fetch_sub(window, Ordering::Relaxed);
            Counters::bump(&self.counters.overload_refusals);
            return Err(overloaded());
        }
        let call_id = state.next_call_id;
        let consumer = ConsumerCore::new(call_id, window, &self.counters);
        state.next_call_id = call_id.checked_add(1).ok_or_else(overloaded)?;
        let owner: Weak<dyn CallOwner> = self.this.clone();
        let guard = CallGuard::new(owner, call_id);
        let completion = || Completion::Stream(StreamCompletion(Arc::clone(&consumer)));

        if local {
            state.pending.insert(
                call_id,
                PendingCall {
                    origin: Origin::Local,
                    endpoint: *endpoint,
                    identity,
                    transmitted: true,
                    earlier_transmitted: false,
                    retained: None,
                    completion: completion(),
                },
            );
            drop(state);
            Counters::bump(&self.counters.streams_opened);
            let sink: Weak<dyn LocalSink> = self.this.clone();
            let context = self.context(
                ReplyRoute::Local { sink, call_id },
                None,
                Some(Outstanding::local(&self.counters)),
            );
            self.admit(
                &Admission {
                    incarnation: endpoint.incarnation(),
                    token: endpoint.token(),
                    identity,
                    stream_window: Some(window),
                    body: &body,
                },
                context,
            );
            return Ok((consumer, guard));
        }

        let frame = request_frame(
            call_id,
            endpoint,
            identity,
            RequestKind {
                flags: REQUEST_FLAG_STREAM,
                stream_window: window,
            },
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
                retained: None,
                completion: completion(),
            },
        );
        drop(state);
        Counters::bump(&self.counters.streams_opened);
        Ok((consumer, guard))
    }

    /// Admission of a stream request passed every check: create its
    /// producer core and bind it to the request's route, within the stream
    /// budgets. The core takes over the reply owed by the context.
    pub(super) fn open_stream(
        &self,
        context: &mut ReplyContext,
        window: u64,
    ) -> Result<(), WireError> {
        let window = window.min(self.config.streams.max_window_bytes);
        let Some(slot) = StreamSlot::acquire(&self.counters, self.config.streams.max_streams)
        else {
            Counters::bump(&self.counters.overload_refusals);
            return Err(WireError::Overloaded);
        };
        let owed = context.outstanding.take();
        let ours = u64::from(self.config.max_frame_bytes);
        let core = match &context.route {
            ReplyRoute::Remote {
                connection,
                call_id,
            } => {
                let Some(live) = connection.upgrade() else {
                    return Err(WireError::Overloaded);
                };
                if live.stream_count() >= self.config.streams.max_streams_per_connection {
                    Counters::bump(&self.counters.overload_refusals);
                    return Err(WireError::Overloaded);
                }
                let limit = live
                    .peer_hello()
                    .map_or(ours, |hello| u64::from(hello.max_frame_bytes).min(ours));
                let core = StreamCore::new(
                    *call_id,
                    CoreRoute::Remote(connection.clone()),
                    window,
                    limit + HEADER_LEN as u64,
                    &self.counters,
                    Box::new((owed, slot)),
                );
                if !live.register_stream(*call_id, &core) {
                    return Err(WireError::Overloaded);
                }
                core
            }
            ReplyRoute::Local { call_id, .. } => {
                let sink: Weak<dyn LocalStreamSink> = self.this.clone();
                let core = StreamCore::new(
                    *call_id,
                    CoreRoute::Local(sink),
                    window,
                    ours + HEADER_LEN as u64,
                    &self.counters,
                    Box::new((owed, slot)),
                );
                // The local consumer acknowledges and cancels straight to it.
                let consumer = self
                    .lock()
                    .pending
                    .get(call_id)
                    .and_then(|call| call.stream().cloned());
                if let Some(consumer) = consumer {
                    consumer.set_route(AckRoute::Local(Arc::downgrade(&core)));
                }
                core
            }
            ReplyRoute::Discard => return Err(WireError::MalformedRequest),
        };
        Counters::bump(&self.counters.streams_admitted);
        context.stream = Some(core);
        Ok(())
    }

    /// The consumer of the pending stream `call_id` from `origin`, if any.
    fn consumer(&self, origin: Origin, call_id: u64) -> Option<Arc<ConsumerCore>> {
        let state = self.lock();
        match state.pending.get(&call_id) {
            Some(call) if call.origin == origin => {
                if let Some(stream) = call.stream() {
                    return Some(Arc::clone(stream));
                }
                drop(state);
                Counters::bump(&self.counters.misrouted_replies);
                None
            }
            Some(_) => {
                drop(state);
                Counters::bump(&self.counters.misrouted_replies);
                None
            }
            None => {
                drop(state);
                // The stream ended or was abandoned on this side: its late
                // frames are discarded.
                Counters::bump(&self.counters.late_stream_frames);
                None
            }
        }
    }

    /// An item of a stream this runtime consumes.
    pub(super) fn stream_item(
        &self,
        origin: Origin,
        route: &AckRoute,
        call_id: u64,
        sequence: u64,
        codec: CodecId,
        body: Vec<u8>,
    ) {
        let Some(consumer) = self.consumer(origin, call_id) else {
            return;
        };
        if let Intook::Violation(violation) = consumer.on_item(route, sequence, codec, body) {
            tracing::debug!(call_id, %violation, "rpc stream refused an item");
            // The consumer ended the stream; stop the producer.
            let removed = self.lock().pending.remove(&call_id);
            drop(removed);
            match route {
                AckRoute::Remote(connection) => {
                    if let Some(connection) = connection.upgrade() {
                        connection.signal_cancel(call_id);
                    }
                }
                AckRoute::Local(producer) => {
                    if let Some(producer) = producer.upgrade() {
                        let _ = producer.cancel();
                    }
                }
            }
        }
    }

    /// The end of a stream this runtime consumes.
    pub(super) fn stream_end(
        &self,
        origin: Origin,
        call_id: u64,
        items: u64,
        error: Option<WireError>,
    ) {
        let Some(consumer) = self.consumer(origin, call_id) else {
            return;
        };
        let call = self.lock().pending.remove(&call_id);
        let Some(call) = call else {
            return;
        };
        let learned = error
            .and_then(permanent_failure)
            .is_some_and(|failure| self.lock().monitor.endpoint_failed(&call.endpoint, failure));
        if learned {
            self.watch.notify();
        }
        let error = error.map(|error| RpcError::from_wire(error, call.identity));
        if let Intook::Violation(violation) = consumer.on_end(items, error) {
            tracing::debug!(call_id, %violation, "rpc stream ended inconsistently");
        }
        drop(call);
    }

    /// A consumption acknowledgement for a stream produced on `connection`.
    pub(super) fn stream_ack(&self, connection: &Connection, call_id: u64, consumed: u64) {
        let Some(core) = connection.stream(call_id) else {
            // The stream already ended or was cancelled (or never existed).
            Counters::bump(&self.counters.stream_acks_ignored);
            return;
        };
        match core.on_ack(consumed) {
            AckEffect::Advanced => Counters::bump(&self.counters.stream_acks_received),
            AckEffect::Ignored => Counters::bump(&self.counters.stream_acks_ignored),
            AckEffect::Violation(violation) => {
                Counters::bump(&self.counters.stream_violations);
                tracing::debug!(call_id, %violation, "rpc stream refused an acknowledgement");
                let _ = core.end_for_violation();
            }
        }
    }

    /// The consumer abandoned a stream produced on `connection`.
    pub(super) fn stream_cancel(&self, connection: &Connection, call_id: u64) {
        match connection.stream(call_id) {
            Some(core) => {
                if core.cancel() {
                    Counters::bump(&self.counters.streams_cancelled);
                }
            }
            None => Counters::bump(&self.counters.stream_acks_ignored),
        }
    }
}

impl<P: Providers> LocalStreamSink for Shared<P> {
    fn local_item(
        &self,
        call_id: u64,
        producer: &Arc<StreamCore>,
        sequence: u64,
        codec: CodecId,
        body: Vec<u8>,
    ) {
        let route = AckRoute::Local(Arc::downgrade(producer));
        self.stream_item(Origin::Local, &route, call_id, sequence, codec, body);
    }

    fn local_end(&self, call_id: u64, items: u64, error: Option<WireError>) {
        self.stream_end(Origin::Local, call_id, items, error);
    }
}
