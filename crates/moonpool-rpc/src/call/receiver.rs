//! Owned request receivers and the bounded queues behind them.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use futures::Stream;
use futures::task::AtomicWaker;

use super::client::ServiceRef;
use super::reply::{ReplyContext, ReplyHandle};
use crate::codec::{CodecId, Wire};
use crate::endpoint::{Endpoint, EndpointToken};
use crate::protocol::{MethodId, RpcMethod, SchemaId, WireError, WireOutcome};

/// Why a mailbox refused an item.
enum MailboxRefusal {
    Full,
    Closed,
}

struct MailboxState<T> {
    items: VecDeque<T>,
    closed: bool,
}

/// A bounded single-consumer queue whose producer never blocks.
struct Mailbox<T> {
    state: Mutex<MailboxState<T>>,
    capacity: usize,
    waker: AtomicWaker,
}

impl<T> Mailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(MailboxState {
                items: VecDeque::new(),
                closed: false,
            }),
            capacity,
            waker: AtomicWaker::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MailboxState<T>> {
        self.state
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    fn push(&self, item: T) -> Result<(), (T, MailboxRefusal)> {
        let mut state = self.lock();
        if state.closed {
            return Err((item, MailboxRefusal::Closed));
        }
        if state.items.len() >= self.capacity {
            return Err((item, MailboxRefusal::Full));
        }
        state.items.push_back(item);
        drop(state);
        self.waker.wake();
        Ok(())
    }

    fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.waker.register(cx.waker());
        let mut state = self.lock();
        if let Some(item) = state.items.pop_front() {
            Poll::Ready(Some(item))
        } else if state.closed {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    /// Close the mailbox and hand back what was queued, so the caller can
    /// drop it outside the lock.
    fn close(&self) -> VecDeque<T> {
        let mut state = self.lock();
        state.closed = true;
        let items = std::mem::take(&mut state.items);
        drop(state);
        self.waker.wake();
        items
    }
}

/// One admitted request: the decoded body and its one-shot responder.
#[derive(Debug)]
pub struct IncomingRequest<M: RpcMethod> {
    /// The decoded request body.
    pub request: M::Request,
    /// The responder, bound to the session the request arrived on.
    pub reply: ReplyHandle<M>,
}

/// A type-erased registered endpoint, as the registry stores it.
pub(crate) trait Inbox: Send + Sync {
    /// The method, schema and request codec the endpoint registered.
    fn identity(&self) -> (MethodId, SchemaId, CodecId);

    /// Decode `body` as the registered request type and queue it with a
    /// responder on `context`. On refusal the rejection has already been
    /// sent along `context`; the error says which.
    fn deliver(&self, body: &[u8], context: ReplyContext) -> Result<(), WireError>;
}

/// Unregisters an endpoint when its receiver is dropped.
pub(crate) trait EndpointOwner: Send + Sync {
    fn unregister(&self, token: EndpointToken);
}

struct TypedInbox<M: RpcMethod> {
    mailbox: Arc<Mailbox<IncomingRequest<M>>>,
}

impl<M: RpcMethod> Inbox for TypedInbox<M> {
    fn identity(&self) -> (MethodId, SchemaId, CodecId) {
        (M::METHOD, M::SCHEMA, <M::Request as Wire>::CODEC)
    }

    fn deliver(&self, body: &[u8], context: ReplyContext) -> Result<(), WireError> {
        let Ok(request) = <M::Request as Wire>::decode(body) else {
            context.deliver(WireOutcome::Err(WireError::MalformedRequest));
            return Err(WireError::MalformedRequest);
        };
        let incoming = IncomingRequest {
            request,
            reply: ReplyHandle::new(context),
        };
        self.mailbox.push(incoming).map_err(|(incoming, refusal)| {
            let error = match refusal {
                MailboxRefusal::Full => WireError::Overloaded,
                MailboxRefusal::Closed => WireError::EndpointNotFound,
            };
            // Refused before admission: the caller gets the rejection, not a
            // broken promise.
            if let Some(context) = incoming.reply.defuse() {
                context.deliver(WireOutcome::Err(error));
            }
            error
        })
    }
}

impl<M: RpcMethod> Drop for TypedInbox<M> {
    fn drop(&mut self) {
        // The registry let go of this endpoint (receiver dropped, or the
        // runtime itself is gone): no new admissions, and queued requests
        // complete as broken promises when `queued` drops.
        let queued = self.mailbox.close();
        drop(queued);
    }
}

/// A receiver whose endpoint token is not known yet.
pub(crate) struct UnboundReceiver<M: RpcMethod> {
    mailbox: Arc<Mailbox<IncomingRequest<M>>>,
}

impl<M: RpcMethod> UnboundReceiver<M> {
    /// Attach the registered identity.
    pub(crate) fn bind(
        self,
        endpoint: Endpoint,
        owner: Weak<dyn EndpointOwner>,
    ) -> RequestStream<M> {
        RequestStream {
            mailbox: self.mailbox,
            service: ServiceRef::new(endpoint),
            owner,
        }
    }
}

/// Build the registry entry and the receiver for a new endpoint.
pub(crate) fn endpoint_pair<M: RpcMethod>(capacity: usize) -> (Arc<dyn Inbox>, UnboundReceiver<M>) {
    let mailbox = Arc::new(Mailbox::new(capacity));
    let inbox: Arc<dyn Inbox> = Arc::new(TypedInbox::<M> {
        mailbox: Arc::clone(&mailbox),
    });
    (inbox, UnboundReceiver { mailbox })
}

/// The owned receiving side of one dynamic endpoint.
///
/// Yields admitted requests in arrival order. Dropping it destroys the
/// endpoint: the registration is removed at once, later requests for its
/// token fail with [`RpcError::EndpointNotFound`](crate::RpcError::EndpointNotFound),
/// and requests queued but not yet taken complete as broken promises. The
/// stream ends when the owning runtime shuts down.
pub struct RequestStream<M: RpcMethod> {
    mailbox: Arc<Mailbox<IncomingRequest<M>>>,
    service: ServiceRef<M>,
    owner: Weak<dyn EndpointOwner>,
}

impl<M: RpcMethod> RequestStream<M> {
    /// The serialisable reference that reaches this endpoint.
    #[must_use]
    pub fn service_ref(&self) -> &ServiceRef<M> {
        &self.service
    }

    /// Receive the next request; `None` once the runtime has shut down.
    pub async fn recv(&mut self) -> Option<IncomingRequest<M>> {
        futures::StreamExt::next(self).await
    }
}

impl<M: RpcMethod> Stream for RequestStream<M> {
    type Item = IncomingRequest<M>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.mailbox.poll_recv(cx)
    }
}

impl<M: RpcMethod> Drop for RequestStream<M> {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.unregister(self.service.endpoint().token());
        }
        let queued = self.mailbox.close();
        drop(queued);
    }
}

impl<M: RpcMethod> std::fmt::Debug for RequestStream<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestStream")
            .field("method", &M::NAME)
            .field("endpoint", self.service.endpoint())
            .finish_non_exhaustive()
    }
}
