//! ActorHost: server-side runtime for virtual actors.
//!
//! The `ActorHost` owns actor instances, spawns processing loops, and handles
//! activation + method dispatch internally. The user registers actor types
//! and the host does the rest.
//!
//! # Orleans Model
//!
//! Turn-based concurrency: one message at a time per actor instance. The
//! processing loop dequeues a message, finds/activates the actor, calls the
//! method, sends the response, then processes the next message.
//!
//! # Usage
//!
//! ```rust,ignore
//! let host = ActorHost::new(transport.clone(), router.clone());
//! host.register::<BankAccountImpl>()?;
//! ```

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::Duration;

use moonpool_sim::assert_sometimes;

use crate::{
    Endpoint, JsonCodec, MessageCodec, NetTransport, Providers, RequestStream, TaskProvider,
    TimeProvider, UID,
};

use super::router::ActorError;
use super::state::ActorStateStore;
use super::types::{ActorId, ActorMessage, ActorResponse, ActorType, CacheInvalidation};
use super::{ActorDirectory, ActorRouter};

/// Maximum number of times a message can be forwarded before being rejected.
/// Prevents infinite forwarding loops.
const MAX_FORWARD_COUNT: u8 = 2;

/// Local `!Send` message queue for routing messages to a per-identity actor task.
///
/// Uses `Rc<RefCell<VecDeque>>` + `Waker` following the same pattern as
/// `NetNotifiedQueue`, but without serialization since messages are already
/// deserialized by the transport layer.
struct IdentityMailbox<C: MessageCodec> {
    inner: RefCell<MailboxInner<C>>,
}

struct MailboxInner<C: MessageCodec> {
    queue: VecDeque<(ActorMessage, crate::ReplyPromise<ActorResponse, C>)>,
    waker: Option<Waker>,
    closed: bool,
}

impl<C: MessageCodec> IdentityMailbox<C> {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            inner: RefCell::new(MailboxInner {
                queue: VecDeque::new(),
                waker: None,
                closed: false,
            }),
        })
    }

    fn send(&self, msg: ActorMessage, reply: crate::ReplyPromise<ActorResponse, C>) {
        let mut inner = self.inner.borrow_mut();
        inner.queue.push_back((msg, reply));
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
    }

    /// Receive the next message, or `None` if the mailbox is closed.
    ///
    /// Returns a `'static` future by cloning the `Rc`, avoiding self-referential
    /// borrow issues in async state machines.
    fn recv(
        mailbox: &Rc<Self>,
    ) -> impl std::future::Future<Output = Option<(ActorMessage, crate::ReplyPromise<ActorResponse, C>)>>
    {
        let mb = Rc::clone(mailbox);
        std::future::poll_fn(move |cx| {
            let mut inner = mb.inner.borrow_mut();
            if let Some(item) = inner.queue.pop_front() {
                return Poll::Ready(Some(item));
            }
            if inner.closed {
                return Poll::Ready(None);
            }
            inner.waker = Some(cx.waker().clone());
            Poll::Pending
        })
    }

    fn close(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.closed = true;
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
    }
}

/// Hint from an actor type about when it should be deactivated.
///
/// Returned by [`ActorHandler::deactivation_hint`] to control the actor's
/// lifetime in memory. This is a per-actor-type setting, not per-message.
///
/// # Orleans Reference
///
/// This is a simplified version of Orleans' `DeactivateOnIdle` /
/// `DelayDeactivation` / `KeepAlive` mechanisms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeactivationHint {
    /// Actor stays in memory between messages (default, backward compatible).
    KeepAlive,
    /// Actor is deactivated after each message dispatch completes.
    /// On deactivation, `on_deactivate` is called and the actor is removed
    /// from memory. The next message will re-activate it (calling
    /// `on_activate` again), loading state from the store.
    DeactivateOnIdle,
    /// Actor is deactivated after the given duration of inactivity.
    ///
    /// The idle timer resets on every message. If no message arrives before
    /// the duration elapses, `on_deactivate` is called and the actor is
    /// removed from memory. The next message re-activates it.
    ///
    /// This is the Orleans default behavior — keeps hot actors cached while
    /// reclaiming memory from cold ones.
    DeactivateAfterIdle(Duration),
}

/// Context provided to actor methods during dispatch.
///
/// Gives the actor access to its own identity, a router for calling
/// other actors, and an optional state store for persistent state.
pub struct ActorContext<P: Providers, C: MessageCodec = JsonCodec> {
    /// The identity of the actor currently being invoked.
    pub id: ActorId,
    /// Router for calling other actors.
    pub router: Rc<ActorRouter<P, C>>,
    state_store: Option<Rc<dyn ActorStateStore>>,
}

impl<P: Providers, C: MessageCodec> ActorContext<P, C> {
    /// Get the state store, if one was configured on the host.
    ///
    /// Returns `None` if the host was created without a state store.
    /// Use this in `on_activate` to load persistent state.
    pub fn state_store(&self) -> Option<&Rc<dyn ActorStateStore>> {
        self.state_store.as_ref()
    }
}

/// Trait implemented by each actor type for method dispatch and lifecycle.
///
/// The host calls `on_activate()` when an actor is first created (or
/// re-activated after deactivation), `dispatch()` for each message, and
/// `on_deactivate()` before removal. The `deactivation_hint()` controls
/// when the actor is removed from memory.
///
/// # Lifecycle
///
/// ```text
/// Default::default() → on_activate() → [dispatch()...] → on_deactivate() → dropped
/// ```
///
/// # Example
///
/// ```rust,ignore
/// #[derive(Default)]
/// struct BankAccount { balance: i64 }
///
/// #[async_trait(?Send)]
/// impl ActorHandler for BankAccount {
///     fn actor_type() -> ActorType { ActorType(0xBA4E_4B00) }
///
///     async fn dispatch(
///         &mut self,
///         _ctx: &ActorContext<impl Providers>,
///         method: u32,
///         body: &[u8],
///     ) -> Result<Vec<u8>, ActorError> {
///         match method {
///             1 => { /* deposit */ }
///             _ => Err(ActorError::UnknownMethod(method)),
///         }
///     }
/// }
/// ```
#[async_trait::async_trait(?Send)]
pub trait ActorHandler: Default + 'static {
    /// The actor type ID (matches the token registered in the transport).
    fn actor_type() -> ActorType;

    /// Hint about when this actor type should be deactivated.
    ///
    /// - `KeepAlive` (default): actor stays in memory between messages.
    /// - `DeactivateOnIdle`: actor is deactivated after each dispatch.
    /// - `DeactivateAfterIdle(duration)`: actor is deactivated after a period
    ///   of inactivity. The timer resets on every message.
    fn deactivation_hint(&self) -> DeactivationHint {
        DeactivationHint::KeepAlive
    }

    /// Called after the actor is created and before the first message.
    ///
    /// Use this to load persistent state from `ctx.state_store()`.
    /// If this returns an error, the actor is not activated and the
    /// triggering message receives an error response.
    async fn on_activate<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
    ) -> Result<(), ActorError> {
        Ok(())
    }

    /// Called before the actor is removed from memory.
    ///
    /// Use this for cleanup. Errors are logged but do not prevent
    /// deactivation.
    async fn on_deactivate<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
    ) -> Result<(), ActorError> {
        Ok(())
    }

    /// Dispatch a method call. The host calls this after looking up the
    /// actor instance.
    ///
    /// # Arguments
    ///
    /// * `ctx` - Context with the actor's identity and a router
    /// * `method` - Method discriminant (1, 2, 3, …)
    /// * `body` - Serialized method-specific request body
    ///
    /// # Returns
    ///
    /// Serialized response body on success, or an error.
    async fn dispatch<P: Providers, C: MessageCodec>(
        &mut self,
        ctx: &ActorContext<P, C>,
        method: u32,
        body: &[u8],
    ) -> Result<Vec<u8>, ActorError>;
}

/// Internal trait to type-erase different actor handler types inside the host.
///
/// Each registered actor type gets a `TypedDispatcher<H>` that implements
/// this trait. The host stores these as `Box<dyn ActorTypeDispatcher<P, C>>`.
trait ActorTypeDispatcher<P: Providers, C: MessageCodec> {
    /// Start the processing loop for this actor type.
    ///
    /// Spawns a task that receives `ActorMessage`s from the transport,
    /// looks up/creates actor instances, and dispatches method calls.
    ///
    /// Returns a closure that closes the request stream, triggering graceful
    /// shutdown of the routing loop and all per-identity tasks.
    fn start(
        &self,
        transport: Rc<NetTransport<P>>,
        router: Rc<ActorRouter<P, C>>,
        directory: Rc<dyn ActorDirectory>,
        state_store: Option<Rc<dyn ActorStateStore>>,
        providers: P,
        pending_tasks: Rc<Cell<usize>>,
    ) -> Box<dyn Fn()>;
}

/// Type-erased dispatcher for a specific actor handler type.
struct TypedDispatcher<H: ActorHandler> {
    _marker: std::marker::PhantomData<H>,
}

impl<H: ActorHandler> TypedDispatcher<H> {
    fn new() -> Self {
        Self {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<H: ActorHandler, P: Providers, C: MessageCodec> ActorTypeDispatcher<P, C>
    for TypedDispatcher<H>
{
    fn start(
        &self,
        transport: Rc<NetTransport<P>>,
        router: Rc<ActorRouter<P, C>>,
        directory: Rc<dyn ActorDirectory>,
        state_store: Option<Rc<dyn ActorStateStore>>,
        providers: P,
        pending_tasks: Rc<Cell<usize>>,
    ) -> Box<dyn Fn()> {
        let actor_type = H::actor_type();
        let token = UID::new(actor_type.0, 0);
        let stream: RequestStream<ActorMessage, C> =
            transport.register_handler(token, router.codec().clone());

        let close_handle = stream.queue();
        pending_tasks.set(pending_tasks.get() + 1);
        providers.task().spawn_task(
            "actor_host_loop",
            actor_processing_loop::<H, P, C>(
                transport,
                router,
                directory,
                state_store,
                stream,
                pending_tasks,
            ),
        );

        Box::new(move || close_handle.close())
    }
}

/// Routing loop for a single actor type.
///
/// Receives `ActorMessage`s from the transport and routes them to per-identity
/// tasks via [`IdentityMailbox`] channels. Each identity gets its own spawned
/// task that owns the actor instance and processes messages sequentially.
///
/// # Turn-Based Concurrency
///
/// One message at a time per actor identity (Orleans guarantee). Different
/// identities process concurrently via separate spawned tasks.
///
/// # Forwarding
///
/// When a message arrives for an actor not hosted locally, and the directory
/// indicates the actor lives on another node, the message is forwarded
/// (up to `MAX_FORWARD_COUNT` hops). The response includes a
/// `CacheInvalidation` hint so the caller can update its stale cache.
async fn actor_processing_loop<H: ActorHandler, P: Providers, C: MessageCodec>(
    transport: Rc<NetTransport<P>>,
    router: Rc<ActorRouter<P, C>>,
    directory: Rc<dyn ActorDirectory>,
    state_store: Option<Rc<dyn ActorStateStore>>,
    stream: RequestStream<ActorMessage, C>,
    pending_tasks: Rc<Cell<usize>>,
) {
    let mut mailboxes: HashMap<String, Rc<IdentityMailbox<C>>> = HashMap::new();
    let actor_type = H::actor_type();
    let local_address = transport.local_address().clone();
    let codec = router.codec().clone();

    loop {
        let Some((actor_msg, reply)) = stream
            .recv_with_transport::<_, ActorResponse>(&transport)
            .await
        else {
            // Stream closed — shutdown. Close all mailboxes so per-identity
            // tasks deactivate their actors and exit.
            for mailbox in mailboxes.values() {
                mailbox.close();
            }
            break;
        };

        let identity = actor_msg.target.identity.clone();

        // Route to existing per-identity mailbox if present
        if let Some(mailbox) = mailboxes.get(&identity) {
            mailbox.send(actor_msg, reply);
            continue;
        }

        // Not local — check directory for forwarding
        let actor_id = ActorId::new(actor_type, identity.clone());
        let dir_lookup = directory.lookup(&actor_id).await;

        match dir_lookup {
            Ok(Some(registered_endpoint)) if registered_endpoint.address != local_address => {
                // Actor is registered on another node — forward
                forward_message::<P, C>(
                    &transport,
                    &actor_msg,
                    &registered_endpoint,
                    &local_address,
                    actor_type,
                    &codec,
                    reply,
                );
            }
            _ => {
                // Actor not registered, or registered on this node — create mailbox + task
                let mailbox = IdentityMailbox::new();
                pending_tasks.set(pending_tasks.get() + 1);
                transport.providers().task().spawn_task(
                    "actor_identity_loop",
                    identity_processing_loop::<H, P, C>(
                        mailbox.clone(),
                        identity.clone(),
                        router.clone(),
                        directory.clone(),
                        state_store.clone(),
                        actor_type,
                        local_address.clone(),
                        transport.providers().time().clone(),
                        pending_tasks.clone(),
                    ),
                );
                mailbox.send(actor_msg, reply);
                mailboxes.insert(identity, mailbox);
            }
        }
    }

    // Signal routing loop completion to the host.
    pending_tasks.set(pending_tasks.get() - 1);
}

/// Per-identity processing loop for a single actor instance.
///
/// Receives messages from the identity's mailbox, activates the actor on first
/// message (or after deactivation), dispatches method calls, and handles
/// deactivation. Runs as a spawned task, maintaining turn-based concurrency
/// for this identity while allowing other identities to process concurrently.
#[allow(clippy::too_many_arguments)]
async fn identity_processing_loop<H: ActorHandler, P: Providers, C: MessageCodec>(
    mailbox: Rc<IdentityMailbox<C>>,
    identity: String,
    router: Rc<ActorRouter<P, C>>,
    directory: Rc<dyn ActorDirectory>,
    state_store: Option<Rc<dyn ActorStateStore>>,
    actor_type: ActorType,
    local_address: crate::NetworkAddress,
    time: P::Time,
    pending_tasks: Rc<Cell<usize>>,
) {
    let mut actor: Option<H> = None;
    let mut was_previously_active = false;

    loop {
        // Wait for next message, with optional idle timeout when actor is active.
        let msg_opt = if let Some(ref a) = actor {
            if let DeactivationHint::DeactivateAfterIdle(duration) = a.deactivation_hint() {
                match time
                    .timeout(duration, IdentityMailbox::recv(&mailbox))
                    .await
                {
                    Ok(msg) => msg,
                    Err(_) => {
                        // Idle timeout elapsed — deactivate the actor
                        let actor_id = ActorId::new(actor_type, identity.clone());
                        if let Some(ref mut a) = actor {
                            let ctx = ActorContext {
                                id: actor_id.clone(),
                                router: router.clone(),
                                state_store: state_store.clone(),
                            };
                            let _ = a.on_deactivate(&ctx).await;
                            assert_sometimes!(true, "deactivate_after_idle_timeout");
                        }
                        actor = None;
                        let _ = directory.unregister(&actor_id).await;
                        continue;
                    }
                }
            } else {
                IdentityMailbox::recv(&mailbox).await
            }
        } else {
            IdentityMailbox::recv(&mailbox).await
        };

        let Some((actor_msg, reply)) = msg_opt else {
            // Mailbox closed — shutdown
            assert_sometimes!(true, "identity_mailbox_closed");
            break;
        };

        // Activate if needed
        if actor.is_none() {
            if was_previously_active {
                assert_sometimes!(true, "actor_reactivated");
            }

            let mut new_actor = H::default();
            let actor_id = ActorId::new(actor_type, identity.clone());
            let ctx = ActorContext {
                id: actor_id.clone(),
                router: router.clone(),
                state_store: state_store.clone(),
            };

            if let Err(e) = new_actor.on_activate(&ctx).await {
                assert_sometimes!(true, "on_activate_failed");
                reply.send(ActorResponse {
                    body: Err(format!("activation failed: {e}")),
                    cache_invalidation: None,
                });
                continue;
            }

            // Register in directory (best-effort)
            let endpoint = Endpoint::new(local_address.clone(), UID::new(actor_type.0, 0));
            let _ = directory.register(&actor_id, endpoint).await;

            actor = Some(new_actor);
            assert_sometimes!(true, "actor_activated");
            was_previously_active = true;
        }

        // Dispatch
        let Some(actor_ref) = actor.as_mut() else {
            reply.send(ActorResponse {
                body: Err("internal: actor not activated".to_string()),
                cache_invalidation: None,
            });
            continue;
        };

        let ctx = ActorContext {
            id: ActorId::new(actor_type, identity.clone()),
            router: router.clone(),
            state_store: state_store.clone(),
        };

        let result = actor_ref
            .dispatch(&ctx, actor_msg.method, &actor_msg.body)
            .await;

        let response = match result {
            Ok(body) => ActorResponse {
                body: Ok(body),
                cache_invalidation: None,
            },
            Err(e) => ActorResponse {
                body: Err(e.to_string()),
                cache_invalidation: None,
            },
        };

        reply.send(response);

        // Check deactivation hint
        if actor
            .as_ref()
            .is_some_and(|a| a.deactivation_hint() == DeactivationHint::DeactivateOnIdle)
        {
            let actor_id = ActorId::new(actor_type, identity.clone());
            if let Some(a) = actor.as_mut() {
                let ctx = ActorContext {
                    id: actor_id.clone(),
                    router: router.clone(),
                    state_store: state_store.clone(),
                };
                let _ = a.on_deactivate(&ctx).await;
                assert_sometimes!(true, "deactivate_on_idle");
            }
            actor = None;
            let _ = directory.unregister(&actor_id).await;
        }
    }

    // Shutdown: deactivate if still active
    if let Some(ref mut a) = actor {
        let actor_id = ActorId::new(actor_type, identity.clone());
        let ctx = ActorContext {
            id: actor_id.clone(),
            router: router.clone(),
            state_store: state_store.clone(),
        };
        let _ = a.on_deactivate(&ctx).await;
        let _ = directory.unregister(&actor_id).await;
    }

    // Signal identity task completion to the host.
    pending_tasks.set(pending_tasks.get() - 1);
}

/// Forward a message to the correct node when the actor lives elsewhere.
///
/// Increments `forward_count` and sends the message to the registered endpoint.
/// The response is proxied back to the original caller with a `CacheInvalidation`
/// hint appended.
fn forward_message<P: Providers, C: MessageCodec>(
    transport: &Rc<NetTransport<P>>,
    actor_msg: &ActorMessage,
    registered_endpoint: &Endpoint,
    local_address: &crate::NetworkAddress,
    actor_type: ActorType,
    codec: &C,
    reply: crate::ReplyPromise<ActorResponse, C>,
) {
    // Check forward count limit
    if actor_msg.forward_count >= MAX_FORWARD_COUNT {
        reply.send(ActorResponse {
            body: Err(format!(
                "message forwarded too many times ({})",
                actor_msg.forward_count
            )),
            cache_invalidation: None,
        });
        return;
    }

    // Build forwarded message with incremented forward_count
    let forwarded_msg = ActorMessage {
        target: actor_msg.target.clone(),
        sender: actor_msg.sender.clone(),
        method: actor_msg.method,
        body: actor_msg.body.clone(),
        forward_count: actor_msg.forward_count + 1,
    };

    // Destination endpoint on the correct node
    let dest = Endpoint::new(
        registered_endpoint.address.clone(),
        UID::new(actor_type.0, 0),
    );

    // Build cache invalidation hint for the caller
    let stale_endpoint = Endpoint::new(local_address.clone(), UID::new(actor_type.0, 0));
    let cache_invalidation = CacheInvalidation {
        actor_id: actor_msg.target.clone(),
        invalid_endpoint: stale_endpoint,
        valid_endpoint: Some(registered_endpoint.clone()),
    };

    // Send the forwarded request
    match crate::send_request(transport, &dest, forwarded_msg, codec.clone()) {
        Ok(future) => {
            // Spawn a task to proxy the response back
            transport.providers().task().spawn_task(
                "actor_forward_proxy",
                proxy_forwarded_response(future, reply, cache_invalidation),
            );
        }
        Err(_e) => {
            reply.send(ActorResponse {
                body: Err("failed to forward message".to_string()),
                cache_invalidation: Some(cache_invalidation),
            });
        }
    }
}

/// Proxy the response from a forwarded message back to the original caller,
/// attaching the cache invalidation hint.
async fn proxy_forwarded_response<C: MessageCodec>(
    future: crate::ReplyFuture<ActorResponse, C>,
    reply: crate::ReplyPromise<ActorResponse, C>,
    cache_invalidation: CacheInvalidation,
) {
    match future.await {
        Ok(mut response) => {
            // Attach cache invalidation hint so caller updates its directory
            response.cache_invalidation = Some(cache_invalidation);
            reply.send(response);
        }
        Err(_e) => {
            reply.send(ActorResponse {
                body: Err("forwarded request failed".to_string()),
                cache_invalidation: Some(cache_invalidation),
            });
        }
    }
}

/// Server-side runtime for virtual actors.
///
/// Owns actor instances, spawns processing loops, and handles activation,
/// lifecycle, and method dispatch internally. Register actor types with
/// `register()`, and the host spawns a processing task for each type.
///
/// # State Persistence
///
/// Optionally configure a state store with [`with_state_store`](Self::with_state_store).
/// Actors can then access it via `ctx.state_store()` in their lifecycle hooks.
///
/// # Example
///
/// ```rust,ignore
/// let host = ActorHost::new(transport.clone(), router.clone(), directory.clone())
///     .with_state_store(Rc::new(InMemoryStateStore::new()));
/// host.register::<BankAccountImpl>();
/// ```
pub struct ActorHost<P: Providers, C: MessageCodec = JsonCodec> {
    /// The transport for registering endpoints.
    transport: Rc<NetTransport<P>>,
    /// Router for actor-to-actor calls (passed to ActorContext).
    router: Rc<ActorRouter<P, C>>,
    /// Directory for registering activated actors.
    directory: Rc<dyn ActorDirectory>,
    /// Optional state store for persistent actor state.
    state_store: Option<Rc<dyn ActorStateStore>>,
    /// Providers bundle for spawning tasks.
    providers: P,
    /// Close handles for registered request streams.
    ///
    /// When called, these close the request stream for each actor type,
    /// causing the routing loop to exit and trigger deactivation.
    close_handles: RefCell<Vec<Box<dyn Fn()>>>,
    /// Number of actor tasks (routing loops + identity tasks) still running.
    ///
    /// Incremented when a routing loop or identity task is spawned,
    /// decremented by each when it exits. Used by `stop()` to wait
    /// for all tasks to complete.
    pending_tasks: Rc<Cell<usize>>,
}

impl<P: Providers, C: MessageCodec> ActorHost<P, C> {
    /// Create a new actor host.
    ///
    /// # Arguments
    ///
    /// * `transport` - The transport for message delivery
    /// * `router` - Router for actor-to-actor calls
    /// * `directory` - Directory for tracking actor locations
    pub fn new(
        transport: Rc<NetTransport<P>>,
        router: Rc<ActorRouter<P, C>>,
        directory: Rc<dyn ActorDirectory>,
    ) -> Self {
        let providers = transport.providers().clone();
        Self {
            transport,
            router,
            directory,
            state_store: None,
            providers,
            close_handles: RefCell::new(Vec::new()),
            pending_tasks: Rc::new(Cell::new(0)),
        }
    }

    /// Configure a state store for persistent actor state.
    ///
    /// When set, actors can access the store via `ctx.state_store()`
    /// in their `on_activate`, `dispatch`, and `on_deactivate` methods.
    pub fn with_state_store(mut self, store: Rc<dyn ActorStateStore>) -> Self {
        self.state_store = Some(store);
        self
    }

    /// Register an actor type with the host.
    ///
    /// This:
    /// 1. Registers a handler in the EndpointMap at `UID::new(actor_type, 0)`
    /// 2. Spawns a processing task that receives `ActorMessage`s and
    ///    dispatches them to the right actor instance and method
    ///
    /// # Type Parameters
    ///
    /// * `H` - The actor handler type (must implement `ActorHandler` + `Default`)
    pub fn register<H: ActorHandler>(&self) {
        let dispatcher = TypedDispatcher::<H>::new();
        let close_handle = dispatcher.start(
            self.transport.clone(),
            self.router.clone(),
            self.directory.clone(),
            self.state_store.clone(),
            self.providers.clone(),
            self.pending_tasks.clone(),
        );
        self.close_handles.borrow_mut().push(close_handle);
    }

    /// Gracefully stop all actor processing loops.
    ///
    /// This:
    /// 1. Closes all request streams (no new messages accepted)
    /// 2. Yields until all routing loop tasks complete, which in turn
    ///    wait for all per-identity tasks to finish deactivation
    ///
    /// After `stop()` returns, all actors have been deactivated via
    /// `on_deactivate` and all processing tasks have exited.
    ///
    /// Calling `stop()` multiple times is safe — close handles are
    /// idempotent and the yield loop exits immediately when no loops
    /// are pending.
    pub async fn stop(&self) {
        // Phase 1: Close all request streams.
        for close_fn in self.close_handles.borrow().iter() {
            close_fn();
        }

        // Phase 2: Yield until all routing loops (and their identity tasks)
        // have finished. Each routing loop decrements pending_tasks on exit.
        while self.pending_tasks.get() > 0 {
            self.providers.task().yield_now().await;
        }
    }
}

impl<P: Providers, C: MessageCodec> Drop for ActorHost<P, C> {
    fn drop(&mut self) {
        // Fire-and-forget: close all request streams so processing loops
        // exit on their next poll. Cannot await JoinHandles in Drop.
        for close_fn in self.close_handles.borrow().iter() {
            close_fn();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use serde::{Deserialize, Serialize};

    use crate::actors::{InMemoryDirectory, LocalPlacement, PlacementStrategy};
    use crate::{Endpoint, NetTransportBuilder, NetworkAddress, TokioProviders};

    use super::*;

    const TEST_ACTOR_TYPE: ActorType = ActorType(0x7E57_AC70);

    mod test_methods {
        pub const INCREMENT: u32 = 1;
        pub const GET_VALUE: u32 = 2;
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct IncrementRequest {
        amount: i64,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct GetValueRequest {}

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct ValueResponse {
        value: i64,
    }

    /// A simple counter actor for testing.
    #[derive(Default)]
    struct CounterActor {
        value: i64,
    }

    #[async_trait::async_trait(?Send)]
    impl ActorHandler for CounterActor {
        fn actor_type() -> ActorType {
            TEST_ACTOR_TYPE
        }

        async fn dispatch<P: Providers, C: MessageCodec>(
            &mut self,
            _ctx: &ActorContext<P, C>,
            method: u32,
            body: &[u8],
        ) -> Result<Vec<u8>, ActorError> {
            let codec = JsonCodec;
            match method {
                test_methods::INCREMENT => {
                    let req: IncrementRequest = codec.decode(body)?;
                    self.value += req.amount;
                    Ok(codec.encode(&ValueResponse { value: self.value })?)
                }
                test_methods::GET_VALUE => Ok(codec.encode(&ValueResponse { value: self.value })?),
                _ => Err(ActorError::UnknownMethod(method)),
            }
        }
    }

    fn test_addr() -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 4700)
    }

    /// Helper to set up the actor host infrastructure for tests.
    fn setup_host() -> (
        Rc<ActorRouter<TokioProviders>>,
        ActorHost<TokioProviders>,
        Rc<dyn ActorDirectory>,
    ) {
        let local_addr = test_addr();
        let providers = TokioProviders::new();
        let transport = NetTransportBuilder::new(providers)
            .local_address(local_addr.clone())
            .build()
            .expect("build transport");

        let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
        let local_endpoint = Endpoint::new(local_addr, UID::new(TEST_ACTOR_TYPE.0, 0));
        let placement: Rc<dyn PlacementStrategy> = Rc::new(LocalPlacement::new(local_endpoint));
        let router = Rc::new(ActorRouter::new(
            transport.clone(),
            directory.clone(),
            placement,
            JsonCodec,
        ));
        let host = ActorHost::new(transport, router.clone(), directory.clone());

        (router, host, directory)
    }

    /// Helper to run async tests with a local runtime that supports spawn_local.
    fn run_local_test<F: std::future::Future<Output = ()> + 'static>(f: F) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(Default::default())
            .expect("build local runtime");
        rt.block_on(f);
    }

    #[test]
    fn test_register_and_send_message() {
        run_local_test(async {
            let (router, host, _directory) = setup_host();
            host.register::<CounterActor>();

            // Give the spawned task a chance to start
            tokio::task::yield_now().await;

            let actor_id = ActorId::new(TEST_ACTOR_TYPE, "test1");
            let resp: ValueResponse = router
                .send_actor_request(
                    &actor_id,
                    test_methods::INCREMENT,
                    &IncrementRequest { amount: 42 },
                )
                .await
                .expect("send_actor_request should succeed");

            assert_eq!(resp, ValueResponse { value: 42 });
        });
    }

    #[test]
    fn test_actor_created_on_first_message() {
        run_local_test(async {
            let (router, host, directory) = setup_host();
            host.register::<CounterActor>();

            tokio::task::yield_now().await;

            // Actor doesn't exist yet in directory
            let actor_id = ActorId::new(TEST_ACTOR_TYPE, "new_actor");
            let lookup = directory.lookup(&actor_id).await.expect("lookup");
            assert!(lookup.is_none());

            // Send message — should activate actor via Default
            let resp: ValueResponse = router
                .send_actor_request(&actor_id, test_methods::GET_VALUE, &GetValueRequest {})
                .await
                .expect("should auto-activate actor");

            // Default value for i64 is 0
            assert_eq!(resp, ValueResponse { value: 0 });
        });
    }

    #[test]
    fn test_state_persists_between_calls() {
        run_local_test(async {
            let (router, host, _directory) = setup_host();
            host.register::<CounterActor>();

            tokio::task::yield_now().await;

            let actor_id = ActorId::new(TEST_ACTOR_TYPE, "persistent");

            // First call: increment by 10
            let resp: ValueResponse = router
                .send_actor_request(
                    &actor_id,
                    test_methods::INCREMENT,
                    &IncrementRequest { amount: 10 },
                )
                .await
                .expect("first increment");
            assert_eq!(resp, ValueResponse { value: 10 });

            // Second call: increment by 5
            let resp: ValueResponse = router
                .send_actor_request(
                    &actor_id,
                    test_methods::INCREMENT,
                    &IncrementRequest { amount: 5 },
                )
                .await
                .expect("second increment");
            assert_eq!(resp, ValueResponse { value: 15 });

            // Third call: get value
            let resp: ValueResponse = router
                .send_actor_request(&actor_id, test_methods::GET_VALUE, &GetValueRequest {})
                .await
                .expect("get value");
            assert_eq!(resp, ValueResponse { value: 15 });
        });
    }

    #[test]
    fn test_unknown_method_returns_error() {
        run_local_test(async {
            let (router, host, _directory) = setup_host();
            host.register::<CounterActor>();

            tokio::task::yield_now().await;

            let actor_id = ActorId::new(TEST_ACTOR_TYPE, "error_test");

            // Send unknown method 99
            let result: Result<ValueResponse, ActorError> = router
                .send_actor_request(&actor_id, 99, &GetValueRequest {})
                .await;

            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(
                err.to_string().contains("unknown method: 99"),
                "expected unknown method error, got: {}",
                err
            );
        });
    }

    #[test]
    fn test_multiple_actors_independent_state() {
        run_local_test(async {
            let (router, host, _directory) = setup_host();
            host.register::<CounterActor>();

            tokio::task::yield_now().await;

            let alice = ActorId::new(TEST_ACTOR_TYPE, "alice");
            let bob = ActorId::new(TEST_ACTOR_TYPE, "bob");

            // Increment alice by 100
            let resp: ValueResponse = router
                .send_actor_request(
                    &alice,
                    test_methods::INCREMENT,
                    &IncrementRequest { amount: 100 },
                )
                .await
                .expect("alice increment");
            assert_eq!(resp.value, 100);

            // Increment bob by 50
            let resp: ValueResponse = router
                .send_actor_request(
                    &bob,
                    test_methods::INCREMENT,
                    &IncrementRequest { amount: 50 },
                )
                .await
                .expect("bob increment");
            assert_eq!(resp.value, 50);

            // Check alice still has 100
            let resp: ValueResponse = router
                .send_actor_request(&alice, test_methods::GET_VALUE, &GetValueRequest {})
                .await
                .expect("alice get");
            assert_eq!(resp.value, 100);

            // Check bob still has 50
            let resp: ValueResponse = router
                .send_actor_request(&bob, test_methods::GET_VALUE, &GetValueRequest {})
                .await
                .expect("bob get");
            assert_eq!(resp.value, 50);
        });
    }

    /// Actor that tracks deactivation via a shared flag.
    struct DeactivationTracker {
        deactivated: Rc<Cell<bool>>,
    }

    impl Default for DeactivationTracker {
        fn default() -> Self {
            Self {
                deactivated: Rc::new(Cell::new(false)),
            }
        }
    }

    const TRACKER_TYPE: ActorType = ActorType(0xDEAC_0001);

    // Thread-local slot to inject the shared flag into the actor on activation.
    thread_local! {
        static DEACTIVATED_FLAG: RefCell<Option<Rc<Cell<bool>>>> = const { RefCell::new(None) };
    }

    #[async_trait::async_trait(?Send)]
    impl ActorHandler for DeactivationTracker {
        fn actor_type() -> ActorType {
            TRACKER_TYPE
        }

        async fn on_activate<P: Providers, C: MessageCodec>(
            &mut self,
            _ctx: &ActorContext<P, C>,
        ) -> Result<(), ActorError> {
            DEACTIVATED_FLAG.with(|f| {
                if let Some(flag) = f.borrow().as_ref() {
                    self.deactivated = flag.clone();
                }
            });
            Ok(())
        }

        async fn on_deactivate<P: Providers, C: MessageCodec>(
            &mut self,
            _ctx: &ActorContext<P, C>,
        ) -> Result<(), ActorError> {
            self.deactivated.set(true);
            Ok(())
        }

        async fn dispatch<P: Providers, C: MessageCodec>(
            &mut self,
            _ctx: &ActorContext<P, C>,
            _method: u32,
            _body: &[u8],
        ) -> Result<Vec<u8>, ActorError> {
            let codec = JsonCodec;
            Ok(codec.encode(&ValueResponse { value: 0 })?)
        }
    }

    #[test]
    fn test_stop_deactivates_actors() {
        run_local_test(async {
            let local_addr = test_addr();
            let providers = TokioProviders::new();
            let transport = NetTransportBuilder::new(providers)
                .local_address(local_addr.clone())
                .build()
                .expect("build transport");

            let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
            let local_endpoint = Endpoint::new(local_addr, UID::new(TRACKER_TYPE.0, 0));
            let placement: Rc<dyn PlacementStrategy> = Rc::new(LocalPlacement::new(local_endpoint));
            let router = Rc::new(ActorRouter::new(
                transport.clone(),
                directory.clone(),
                placement,
                JsonCodec,
            ));

            let host = ActorHost::new(transport, router.clone(), directory.clone());
            host.register::<DeactivationTracker>();

            // Inject the shared flag before sending a message
            let deactivated = Rc::new(Cell::new(false));
            DEACTIVATED_FLAG.with(|f| {
                *f.borrow_mut() = Some(deactivated.clone());
            });

            tokio::task::yield_now().await;

            // Send a message to activate the actor
            let actor_id = ActorId::new(TRACKER_TYPE, "tracker1");
            let _resp: ValueResponse = router
                .send_actor_request(&actor_id, 1, &GetValueRequest {})
                .await
                .expect("activate actor");

            assert!(!deactivated.get(), "should not be deactivated yet");

            // Stop the host — should deactivate all actors
            host.stop().await;

            assert!(deactivated.get(), "actor should have been deactivated");
        });
    }
}
