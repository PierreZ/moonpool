//! One-shot reply handles and their peer-relative routes.

use std::marker::PhantomData;
use std::sync::{Arc, Weak};

use crate::codec::{Wire, encode_to_vec};
use crate::protocol::{
    RpcMethod, WireError, WireMessage, WireOutcome, encode_frame, encode_message,
    reply_envelope_len,
};
use crate::stats::Counters;
use crate::transport::connection::Connection;
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
}

/// Everything a reply needs besides its value.
pub(crate) struct ReplyContext {
    pub(crate) route: ReplyRoute,
    pub(crate) counters: Arc<Counters>,
    pub(crate) max_frame_bytes: u32,
    /// Where the request came from (`None` for a local caller).
    pub(crate) peer: Option<PeerContext>,
}

impl ReplyContext {
    /// Deliver an outcome along the route. Returns whether a live route
    /// accepted it.
    pub(crate) fn deliver(self, outcome: WireOutcome) -> bool {
        // Local and remote replies obey the same frame limit, so a caller
        // cannot tell the two routes apart by size.
        let outcome = match outcome {
            WireOutcome::Ok { body, .. }
                if reply_envelope_len(body.len()) as u64 > u64::from(self.max_frame_bytes) =>
            {
                WireOutcome::Err(WireError::ReplyTooLarge)
            }
            outcome => outcome,
        };
        let delivered = match self.route {
            ReplyRoute::Local { sink, call_id } => match sink.upgrade() {
                Some(sink) => {
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
                    let payload = encode_message(&WireMessage::Reply { call_id, outcome });
                    if let Ok(frame) = encode_frame(&payload, self.max_frame_bytes) {
                        connection.push_control(frame)
                    } else {
                        // Not even a rejection fits the frame limit: close
                        // the session so the caller observes a disconnect
                        // rather than waiting on a reply that never comes.
                        connection.close();
                        false
                    }
                }
                None => false,
            },
        };
        if delivered {
            Counters::bump(&self.counters.replies_sent);
        } else {
            Counters::bump(&self.counters.replies_dropped);
        }
        delivered
    }
}

/// The one-shot responder for one admitted request.
///
/// Reply with [`send`](Self::send). Dropping the handle without replying
/// completes the call with [`ErrorReason::BrokenPromise`](crate::ErrorReason::BrokenPromise):
/// the caller learns the request was admitted and abandoned.
///
/// A reply handle is not a service reference: it has no byte form and
/// cannot be forwarded to a third party. Its route is the connection
/// (session) the request arrived on, or the local caller, fixed when the
/// request was decoded.
#[must_use = "dropping a reply handle without replying breaks the caller's promise"]
pub struct ReplyHandle<M: RpcMethod> {
    context: Option<ReplyContext>,
    _method: PhantomData<fn(M::Reply)>,
}

impl<M: RpcMethod> ReplyHandle<M> {
    pub(crate) fn new(context: ReplyContext) -> Self {
        Self {
            context: Some(context),
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

    /// Reply to the caller.
    ///
    /// Returns whether the reply was handed to a live route. `true` is not a
    /// delivery acknowledgement: the connection may still fail before the
    /// caller reads it. `false` means the route was already gone (the caller
    /// disconnected or its runtime stopped).
    pub fn send(mut self, reply: &M::Reply) -> bool {
        let Some(context) = self.context.take() else {
            return false;
        };
        let outcome = match encode_to_vec(reply) {
            Ok(body) => WireOutcome::Ok {
                codec: <M::Reply as Wire>::CODEC,
                body,
            },
            Err(_) => WireOutcome::Err(WireError::ReplyTooLarge),
        };
        context.deliver(outcome)
    }

    /// Detach the context without replying (admission refused the request).
    pub(crate) fn defuse(mut self) -> Option<ReplyContext> {
        self.context.take()
    }
}

impl<M: RpcMethod> Drop for ReplyHandle<M> {
    fn drop(&mut self) {
        if let Some(context) = self.context.take() {
            Counters::bump(&context.counters.broken_promises);
            context.deliver(WireOutcome::Err(WireError::BrokenPromise));
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
