//! Owned request receivers and the bounded queues behind them.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use futures::Stream;
use futures::task::AtomicWaker;

use super::reply::{ReplyContext, ReplyHandle};
use crate::codec::{CodecId, Wire};
use crate::endpoint::EndpointToken;
use crate::error::ErrorReason;
use crate::interface::ServiceRef;
use crate::protocol::{MethodId, RpcMethod, SchemaVersion, WireError};

/// Why a mailbox refused an item.
enum MailboxRefusal {
    Full,
    Closed,
}

struct MailboxState<T> {
    /// Each item with the bytes it counts against the byte budget.
    items: VecDeque<(T, u64)>,
    bytes: u64,
    closed: bool,
}

/// A bounded single-consumer queue whose producer never blocks: it holds
/// at most `capacity` items and `byte_capacity` bytes.
struct Mailbox<T> {
    state: Mutex<MailboxState<T>>,
    capacity: usize,
    byte_capacity: u64,
    waker: AtomicWaker,
}

impl<T> Mailbox<T> {
    fn new((capacity, byte_capacity): (usize, u64)) -> Self {
        Self {
            state: Mutex::new(MailboxState {
                items: VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            capacity,
            byte_capacity,
            waker: AtomicWaker::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MailboxState<T>> {
        self.state
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    fn push(&self, item: T, bytes: u64) -> Result<(), (T, MailboxRefusal)> {
        let mut state = self.lock();
        if state.closed {
            return Err((item, MailboxRefusal::Closed));
        }
        if state.items.len() >= self.capacity
            || state.bytes.saturating_add(bytes) > self.byte_capacity
        {
            return Err((item, MailboxRefusal::Full));
        }
        state.bytes = state.bytes.saturating_add(bytes);
        state.items.push_back((item, bytes));
        drop(state);
        self.waker.wake();
        Ok(())
    }

    fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.waker.register(cx.waker());
        let mut state = self.lock();
        if let Some((item, bytes)) = state.items.pop_front() {
            state.bytes -= bytes.min(state.bytes);
            Poll::Ready(Some(item))
        } else if state.closed {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    /// Close the mailbox and hand back what was queued, so the caller can
    /// drop it outside the lock.
    fn close(&self) -> VecDeque<(T, u64)> {
        let mut state = self.lock();
        state.closed = true;
        state.bytes = 0;
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
    fn identity(&self) -> (MethodId, SchemaVersion, CodecId);

    /// Whether the method answers with a reply stream.
    fn streaming(&self) -> bool;

    /// Decode `body` as the registered request type and queue it with a
    /// responder on `context`. On refusal the rejection has already been
    /// sent along `context`; the error says which.
    fn deliver(&self, body: &[u8], context: ReplyContext) -> Result<(), WireError>;
}

/// The runtime side of a registration: implemented by the runtime's
/// shared state, held weakly by receivers and groups.
pub(crate) trait EndpointOwner: Send + Sync {
    /// Destroy the registration under `token` (`method: None`), or only one
    /// method of a group.
    fn unregister(&self, token: EndpointToken, method: Option<MethodId>);

    /// Add a method's inbox to the live group under `token`.
    fn attach(
        &self,
        token: EndpointToken,
        method: MethodId,
        inbox: Arc<dyn Inbox>,
    ) -> Result<(), ErrorReason>;

    /// The capacity of each endpoint's request queue: requests and bytes.
    fn queue_capacity(&self) -> (usize, u64);
}

struct TypedInbox<M: RpcMethod> {
    mailbox: Arc<Mailbox<IncomingRequest<M>>>,
}

impl<M: RpcMethod> Inbox for TypedInbox<M> {
    fn identity(&self) -> (MethodId, SchemaVersion, CodecId) {
        (M::METHOD, M::SCHEMA, <M::Request as Wire>::CODEC)
    }

    fn streaming(&self) -> bool {
        M::STREAMING
    }

    fn deliver(&self, body: &[u8], context: ReplyContext) -> Result<(), WireError> {
        let Ok(request) = <M::Request as Wire>::decode(body) else {
            context.reject(WireError::MalformedRequest);
            return Err(WireError::MalformedRequest);
        };
        let incoming = IncomingRequest {
            request,
            reply: ReplyHandle::new(context),
        };
        self.mailbox
            .push(incoming, body.len() as u64)
            .map_err(|(incoming, refusal)| {
                let error = match refusal {
                    MailboxRefusal::Full => WireError::Overloaded,
                    MailboxRefusal::Closed => WireError::EndpointNotFound,
                };
                // Refused before admission: the caller gets the rejection, not a
                // broken promise.
                if let Some(context) = incoming.reply.defuse() {
                    context.reject(error);
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
    ///
    /// `grouped` streams remove only their own method when dropped; the
    /// others destroy the whole registration.
    pub(crate) fn bind(
        self,
        service: ServiceRef<M>,
        owner: Weak<dyn EndpointOwner>,
        grouped: bool,
    ) -> RequestStream<M> {
        RequestStream {
            mailbox: self.mailbox,
            service,
            owner,
            grouped,
        }
    }
}

/// Build the registry entry and the receiver for a new endpoint.
pub(crate) fn endpoint_pair<M: RpcMethod>(
    capacity: (usize, u64),
) -> (Arc<dyn Inbox>, UnboundReceiver<M>) {
    let mailbox = Arc::new(Mailbox::new(capacity));
    let inbox: Arc<dyn Inbox> = Arc::new(TypedInbox::<M> {
        mailbox: Arc::clone(&mailbox),
    });
    (inbox, UnboundReceiver { mailbox })
}

/// The owned receiving side of one method of a dynamic endpoint: the
/// pull-style handler primitive.
///
/// Yields admitted requests in arrival order, one-way requests included
/// (their [`ReplyHandle::expects_reply`] is `false`). Dropping a stream
/// from [`RpcHandle::register`](crate::RpcHandle::register) destroys the
/// endpoint: the registration is removed at once, later requests for its
/// token fail with [`ErrorReason::EndpointNotFound`](crate::ErrorReason::EndpointNotFound),
/// and requests queued but not yet taken complete as broken promises.
/// Dropping a stream from [`ServiceGroup::serve`](crate::ServiceGroup::serve)
/// removes only its method from the group
/// ([`ErrorReason::MethodNotFound`](crate::ErrorReason::MethodNotFound)
/// afterwards). The stream ends when its group is dropped or the owning
/// runtime shuts down.
pub struct RequestStream<M: RpcMethod> {
    mailbox: Arc<Mailbox<IncomingRequest<M>>>,
    service: ServiceRef<M>,
    owner: Weak<dyn EndpointOwner>,
    grouped: bool,
}

impl<M: RpcMethod> RequestStream<M> {
    /// The serialisable reference that reaches this endpoint.
    #[must_use]
    pub fn service_ref(&self) -> &ServiceRef<M> {
        &self.service
    }

    /// Receive the next request; `None` once the endpoint is gone (its
    /// group was dropped or the runtime shut down).
    pub async fn recv(&mut self) -> Option<IncomingRequest<M>> {
        futures::StreamExt::next(self).await
    }

    /// Poll for the next request without pinning (the stream is `Unpin`):
    /// the building block for dispatchers that multiplex several streams.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<IncomingRequest<M>>> {
        self.mailbox.poll_recv(cx)
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
            let method = self.grouped.then_some(M::METHOD);
            owner.unregister(self.service.endpoint().token(), method);
        }
        let queued = self.mailbox.close();
        drop(queued);
    }
}

impl<M: RpcMethod> std::fmt::Debug for RequestStream<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestStream")
            .field("method", &M::NAME)
            .field("endpoint", &self.service.endpoint())
            .finish_non_exhaustive()
    }
}
