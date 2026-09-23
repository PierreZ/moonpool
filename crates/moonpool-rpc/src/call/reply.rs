//! One-shot reply handles and their peer-relative routes.

use std::marker::PhantomData;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use crate::codec::{Wire, encode_to_vec};
use crate::config::ResourceLimits;
use crate::protocol::{
    RpcMethod, WireError, WireMessage, WireOutcome, encode_frame, encode_message,
    reply_envelope_len,
};
use crate::stats::Counters;
use crate::stream::producer::StreamCore;
use crate::stream::{SendError, StreamProducer};
use crate::transport::connection::{CloseReason, Connection};
use crate::transport::upgrade::PeerContext;

/// Completes local calls: implemented by the runtime's shared state.
pub(crate) trait LocalSink: Send + Sync {
    fn complete_local(&self, call_id: u64, outcome: WireOutcome);
}

/// Where a reply goes. A remote route is bound to the one connection
/// (session) that delivered the request: if that connection is gone the
/// reply is dropped, never redirected to a newer connection or another
/// process that happens to reuse the address.
pub(crate) enum ReplyRoute {
    /// Back over the delivering connection, under the caller's call id.
    Remote {
        connection: Weak<Connection>,
        call_id: u64,
    },
    /// Straight to a pending call of this runtime.
    Local {
        sink: Weak<dyn LocalSink>,
        call_id: u64,
    },
    /// A one-way request: nothing is ever sent back.
    Discard,
}

/// One reply owed, for as long as this value lives: counted on the
/// connection the request came from (idleness and the per-connection
/// in-flight budget) and in the runtime (the runtime in-flight budget).
/// A queued reply carries it until it is written.
pub(crate) struct Outstanding {
    connection: Option<Weak<Connection>>,
    counters: Arc<Counters>,
    /// The connection's count including this one, when it was taken.
    on_connection: usize,
    /// The runtime's count including this one, when it was taken.
    in_runtime: usize,
}

impl Outstanding {
    /// A reply owed over `connection`.
    pub(crate) fn remote(connection: &Arc<Connection>, counters: &Arc<Counters>) -> Self {
        let on_connection = connection.begin_outstanding();
        let in_runtime = counters.inflight_requests.fetch_add(1, Ordering::Relaxed) + 1;
        Self {
            connection: Some(Arc::downgrade(connection)),
            counters: Arc::clone(counters),
            on_connection,
            in_runtime,
        }
    }

    /// A reply owed to a caller in this runtime.
    pub(crate) fn local(counters: &Arc<Counters>) -> Self {
        let in_runtime = counters.inflight_requests.fetch_add(1, Ordering::Relaxed) + 1;
        Self {
            connection: None,
            counters: Arc::clone(counters),
            on_connection: 0,
            in_runtime,
        }
    }

    /// Whether admitting this request exceeds an in-flight budget.
    pub(crate) fn over(&self, limits: &ResourceLimits) -> bool {
        self.on_connection > limits.max_inflight_per_connection
            || self.in_runtime > limits.max_inflight_requests
    }
}

impl Drop for Outstanding {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.as_ref().and_then(Weak::upgrade) {
            connection.end_outstanding();
        }
        self.counters
            .inflight_requests
            .fetch_sub(1, Ordering::Relaxed);
    }
}

/// Everything a reply needs besides its value.
pub(crate) struct ReplyContext {
    pub(crate) route: ReplyRoute,
    pub(crate) counters: Arc<Counters>,
    pub(crate) max_frame_bytes: u32,
    /// Where the request came from (`None` for a local caller).
    pub(crate) peer: Option<PeerContext>,
    /// The reply owed (`None` for a one-way request, and for a stream,
    /// whose producer holds it instead).
    pub(crate) outstanding: Option<Outstanding>,
    /// The reply stream the request opened, once admitted.
    pub(crate) stream: Option<Arc<StreamCore>>,
}

impl ReplyContext {
    /// Refuse the request before admission: a rejection, queued as a
    /// control frame. A stream opened for it is discarded. Returns whether
    /// a live route accepted it.
    pub(crate) fn reject(mut self, error: WireError) -> bool {
        if let Some(stream) = self.stream.take() {
            let _ = stream.cancel();
        }
        self.deliver(WireOutcome::Err(error), false)
    }

    /// Complete an admitted request: queued as a reply in the room its
    /// admission reserved.
    fn complete(self, outcome: WireOutcome) -> bool {
        self.deliver(outcome, true)
    }

    fn deliver(self, outcome: WireOutcome, admitted: bool) -> bool {
        let Self {
            route,
            counters,
            max_frame_bytes,
            outstanding,
            ..
        } = self;
        let delivered = match route {
            // Nothing to send and nobody to tell: neither sent nor dropped.
            ReplyRoute::Discard => return false,
            ReplyRoute::Local { sink, call_id } => match sink.upgrade() {
                Some(sink) => {
                    // Local replies obey the local frame limit, as a remote
                    // caller with the same limit would.
                    let outcome = bounded(outcome, max_frame_bytes);
                    sink.complete_local(call_id, outcome);
                    true
                }
                None => false,
            },
            ReplyRoute::Remote {
                connection,
                call_id,
            } => match connection.upgrade() {
                Some(connection) => {
                    // The session limit is the smaller of ours and the
                    // peer's: an oversized reply fails its own call with
                    // ReplyTooLarge and the session stays up.
                    let limit = connection.peer_hello().map_or(max_frame_bytes, |hello| {
                        hello.max_frame_bytes.min(max_frame_bytes)
                    });
                    let outcome = bounded(outcome, limit);
                    let payload = encode_message(&WireMessage::Reply { call_id, outcome });
                    match (encode_frame(&payload, limit), outstanding) {
                        (Ok(frame), Some(owed)) if admitted => {
                            connection.push_reply(frame, Box::new(owed)).is_ok()
                        }
                        (Ok(frame), _) => connection.push_control(frame),
                        (Err(_), _) => {
                            // Not even a rejection fits the frame limit:
                            // close the session so the caller observes a
                            // disconnect rather than waiting forever.
                            let _ = connection.close(CloseReason::Local);
                            false
                        }
                    }
                }
                None => false,
            },
        };
        if delivered {
            Counters::bump(&counters.replies_sent);
        } else {
            Counters::bump(&counters.replies_dropped);
        }
        delivered
    }
}

/// Replace an ok outcome that would not fit `limit` with `ReplyTooLarge`.
fn bounded(outcome: WireOutcome, limit: u32) -> WireOutcome {
    match outcome {
        WireOutcome::Ok { body, .. }
            if reply_envelope_len(body.len()) as u64 > u64::from(limit) =>
        {
            WireOutcome::Err(WireError::ReplyTooLarge)
        }
        outcome => outcome,
    }
}

/// The one-shot responder for one admitted request.
///
/// Reply with [`send`](Self::send). Dropping the handle without replying
/// completes the call with [`ErrorReason::BrokenPromise`](crate::ErrorReason::BrokenPromise):
/// the caller learns the request was admitted and abandoned. To decline to
/// answer on purpose, call [`never_reply`](Self::never_reply): nothing is
/// sent, and the caller keeps waiting under its own deadline, failure bound
/// or cancellation.
///
/// For a streaming method ([`RpcMethod::STREAMING`]) the handle answers
/// with a stream: [`into_stream`](Self::into_stream) returns the
/// [`StreamProducer`]. `send` then sends one item and ends the stream.
///
/// A reply handle is not a service reference: it has no byte form and
/// cannot be forwarded to a third party. Its route is the connection
/// (session) the request arrived on, or the local caller, fixed when the
/// request was decoded.
#[must_use = "dropping a reply handle without replying breaks the caller's promise"]
pub struct ReplyHandle<M: RpcMethod> {
    // Boxed: a handle is moved around with its request, and handed back by
    // `into_stream`.
    context: Option<Box<ReplyContext>>,
    _method: PhantomData<fn(M::Reply)>,
}

impl<M: RpcMethod> ReplyHandle<M> {
    pub(crate) fn new(context: ReplyContext) -> Self {
        Self {
            context: Some(Box::new(context)),
            _method: PhantomData,
        }
    }

    /// What the session upgrade established about the peer whose
    /// connection delivered the request, or `None` for a caller in this
    /// runtime.
    #[must_use]
    pub fn peer(&self) -> Option<&PeerContext> {
        self.context
            .as_ref()
            .and_then(|context| context.peer.as_ref())
    }

    /// Whether anyone waits for a reply: `false` for a one-way request.
    #[must_use]
    pub fn expects_reply(&self) -> bool {
        self.context
            .as_ref()
            .is_some_and(|context| !matches!(context.route, ReplyRoute::Discard))
    }

    /// Whether the caller opened a reply stream (the method is
    /// [`STREAMING`](RpcMethod::STREAMING)).
    #[must_use]
    pub fn is_stream(&self) -> bool {
        self.context
            .as_ref()
            .is_some_and(|context| context.stream.is_some())
    }

    /// Answer with a reply stream: the producer of the stream the caller
    /// opened. Returns the handle back for a request that did not open
    /// one (a unary method's).
    ///
    /// # Errors
    ///
    /// The handle itself, when the request is not a stream request.
    pub fn into_stream(mut self) -> Result<StreamProducer<M>, Self> {
        let stream = self
            .context
            .as_mut()
            .and_then(|context| context.stream.take());
        match (stream, self.context.take()) {
            (Some(core), Some(context)) => Ok(StreamProducer::new(
                core,
                context.peer,
                Arc::clone(&context.counters),
            )),
            (_, context) => {
                self.context = context;
                Err(self)
            }
        }
    }

    /// Finish without replying and without breaking the promise.
    ///
    /// Nothing is sent. The caller is not told the request was abandoned: it
    /// keeps waiting until its own deadline, failure bound or cancellation
    /// ends the call (a caller without one waits until its connection
    /// ends). For a stream the stream stays open, empty, until the caller
    /// abandons it or the connection ends. Use it for requests that are
    /// answered some other way, or deliberately never.
    pub fn never_reply(mut self) {
        if let Some(context) = self.context.take() {
            Counters::bump(&context.counters.explicit_no_replies);
            // Dropping the context releases the owed-reply count.
            drop(context);
        }
    }

    /// Reply to the caller (for a stream: send `reply` as its only item and
    /// end it).
    ///
    /// Returns whether the reply was handed to a live route. `true` is not a
    /// delivery acknowledgement: the connection may still fail before the
    /// caller reads it. `false` means the route was already gone (the caller
    /// disconnected or its runtime stopped) or the request was one-way.
    pub fn send(mut self, reply: &M::Reply) -> bool {
        let Some(mut context) = self.context.take() else {
            return false;
        };
        if let Some(core) = context.stream.take() {
            let producer = StreamProducer::<M>::new(core, None, Arc::clone(&context.counters));
            // The first item always has the whole window: only its size or
            // its encoding can stop it, and the caller is told which.
            return match producer.try_send(reply) {
                Ok(true) => producer.finish(),
                Err(SendError::Encode(_)) => {
                    producer.end_with(WireError::ReplyEncodeFailed);
                    false
                }
                Ok(false) | Err(_) => {
                    producer.end_with(WireError::ReplyTooLarge);
                    false
                }
            };
        }
        let outcome = match encode_to_vec(reply) {
            Ok(body) => WireOutcome::Ok {
                codec: <M::Reply as Wire>::CODEC,
                body,
            },
            Err(_) => WireOutcome::Err(WireError::ReplyEncodeFailed),
        };
        (*context).complete(outcome)
    }

    /// Detach the context without replying (admission refused the request).
    pub(crate) fn defuse(mut self) -> Option<ReplyContext> {
        self.context.take().map(|context| *context)
    }
}

impl<M: RpcMethod> Drop for ReplyHandle<M> {
    fn drop(&mut self) {
        if let Some(mut context) = self.context.take() {
            if matches!(context.route, ReplyRoute::Discard) {
                return;
            }
            Counters::bump(&context.counters.broken_promises);
            if let Some(core) = context.stream.take() {
                let _ = core.end(Some(WireError::BrokenPromise));
                return;
            }
            (*context).complete(WireOutcome::Err(WireError::BrokenPromise));
        }
    }
}

impl<M: RpcMethod> std::fmt::Debug for ReplyHandle<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyHandle")
            .field("method", &M::NAME)
            .field("pending", &self.context.is_some())
            .finish()
    }
}
