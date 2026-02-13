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

use std::collections::HashMap;
use std::rc::Rc;

use crate::{
    Endpoint, JsonCodec, MessageCodec, NetTransport, Providers, RequestStream, TaskProvider, UID,
};

use super::router::ActorError;
use super::state::ActorStateStore;
use super::types::{ActorId, ActorMessage, ActorResponse, ActorType, CacheInvalidation};
use super::{ActorDirectory, ActorRouter};

/// Maximum number of times a message can be forwarded before being rejected.
/// Prevents infinite forwarding loops.
const MAX_FORWARD_COUNT: u8 = 2;

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
    fn start(
        &self,
        transport: Rc<NetTransport<P>>,
        router: Rc<ActorRouter<P, C>>,
        directory: Rc<dyn ActorDirectory>,
        state_store: Option<Rc<dyn ActorStateStore>>,
        providers: P,
    );
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
    ) {
        let actor_type = H::actor_type();
        let token = UID::new(actor_type.0, 0);
        let stream: RequestStream<ActorMessage, C> =
            transport.register_handler(token, router.codec().clone());

        providers.task().spawn_task(
            "actor_host_loop",
            actor_processing_loop::<H, P, C>(transport, router, directory, state_store, stream),
        );
    }
}

/// Processing loop for a single actor type.
///
/// Receives `ActorMessage`s from the transport, looks up or creates the
/// target actor instance, calls `dispatch()`, and sends the response.
///
/// # Turn-Based Concurrency
///
/// One message at a time per actor instance. The loop processes messages
/// sequentially — no concurrent access to the same actor.
///
/// # Lifecycle
///
/// When an actor is first activated: `Default::default()` → `on_activate()`.
/// After each dispatch, if `deactivation_hint()` returns `DeactivateOnIdle`,
/// the actor is deactivated: `on_deactivate()` → removed from memory.
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
) {
    let mut actors: HashMap<String, H> = HashMap::new();
    let actor_type = H::actor_type();
    let local_address = transport.local_address().clone();
    let codec = router.codec().clone();

    loop {
        let Some((actor_msg, reply)) = stream
            .recv_with_transport::<_, ActorResponse>(&transport)
            .await
        else {
            // Stream closed — shutdown. Deactivate all remaining actors.
            deactivate_all::<H, P, C>(&mut actors, &router, &directory, &state_store, actor_type)
                .await;
            break;
        };

        let identity = actor_msg.target.identity.clone();

        // Check if we host this actor locally
        if actors.contains_key(&identity) {
            // Actor exists locally — dispatch directly
            dispatch_local::<H, P, C>(
                &mut actors,
                &identity,
                &actor_msg,
                &router,
                &state_store,
                actor_type,
                reply,
            )
            .await;

            // Check deactivation hint
            maybe_deactivate::<H, P, C>(
                &mut actors,
                &identity,
                &router,
                &directory,
                &state_store,
                actor_type,
            )
            .await;
            continue;
        }

        // Actor not local — check directory
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
                // Actor not registered, or registered on this node — activate locally
                let mut actor = H::default();
                let ctx = ActorContext {
                    id: ActorId::new(actor_type, identity.clone()),
                    router: router.clone(),
                    state_store: state_store.clone(),
                };

                // Call on_activate — if it fails, send error response and skip
                if let Err(e) = actor.on_activate(&ctx).await {
                    reply.send(ActorResponse {
                        body: Err(format!("activation failed: {e}")),
                        cache_invalidation: None,
                    });
                    continue;
                }

                actors.insert(identity.clone(), actor);

                // Register in directory (best-effort, ignore race conditions)
                let endpoint = Endpoint::new(local_address.clone(), UID::new(actor_type.0, 0));
                let _ = directory.register(&actor_id, endpoint).await;

                dispatch_local::<H, P, C>(
                    &mut actors,
                    &identity,
                    &actor_msg,
                    &router,
                    &state_store,
                    actor_type,
                    reply,
                )
                .await;

                // Check deactivation hint
                maybe_deactivate::<H, P, C>(
                    &mut actors,
                    &identity,
                    &router,
                    &directory,
                    &state_store,
                    actor_type,
                )
                .await;
            }
        }
    }
}

/// Dispatch a method call to a locally hosted actor.
async fn dispatch_local<H: ActorHandler, P: Providers, C: MessageCodec>(
    actors: &mut HashMap<String, H>,
    identity: &str,
    actor_msg: &ActorMessage,
    router: &Rc<ActorRouter<P, C>>,
    state_store: &Option<Rc<dyn ActorStateStore>>,
    actor_type: ActorType,
    reply: crate::ReplyPromise<ActorResponse, C>,
) {
    let Some(actor) = actors.get_mut(identity) else {
        reply.send(ActorResponse {
            body: Err("actor not found after activation".to_string()),
            cache_invalidation: None,
        });
        return;
    };

    let ctx = ActorContext {
        id: ActorId::new(actor_type, identity.to_string()),
        router: router.clone(),
        state_store: state_store.clone(),
    };

    let result = actor
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
}

/// Check the deactivation hint and deactivate the actor if needed.
async fn maybe_deactivate<H: ActorHandler, P: Providers, C: MessageCodec>(
    actors: &mut HashMap<String, H>,
    identity: &str,
    router: &Rc<ActorRouter<P, C>>,
    directory: &Rc<dyn ActorDirectory>,
    state_store: &Option<Rc<dyn ActorStateStore>>,
    actor_type: ActorType,
) {
    let should_deactivate = actors
        .get(identity)
        .is_some_and(|a| a.deactivation_hint() == DeactivationHint::DeactivateOnIdle);

    if !should_deactivate {
        return;
    }

    let actor_id = ActorId::new(actor_type, identity.to_string());

    // Call on_deactivate (best-effort)
    if let Some(actor) = actors.get_mut(identity) {
        let ctx = ActorContext {
            id: actor_id.clone(),
            router: router.clone(),
            state_store: state_store.clone(),
        };
        let _ = actor.on_deactivate(&ctx).await;
    }

    // Remove from memory
    actors.remove(identity);

    // Unregister from directory (best-effort)
    let _ = directory.unregister(&actor_id).await;
}

/// Deactivate all actors during shutdown.
async fn deactivate_all<H: ActorHandler, P: Providers, C: MessageCodec>(
    actors: &mut HashMap<String, H>,
    router: &Rc<ActorRouter<P, C>>,
    directory: &Rc<dyn ActorDirectory>,
    state_store: &Option<Rc<dyn ActorStateStore>>,
    actor_type: ActorType,
) {
    let identities: Vec<String> = actors.keys().cloned().collect();
    for identity in identities {
        let actor_id = ActorId::new(actor_type, identity.clone());
        if let Some(actor) = actors.get_mut(&identity) {
            let ctx = ActorContext {
                id: actor_id.clone(),
                router: router.clone(),
                state_store: state_store.clone(),
            };
            let _ = actor.on_deactivate(&ctx).await;
        }
        actors.remove(&identity);
        let _ = directory.unregister(&actor_id).await;
    }
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
        dispatcher.start(
            self.transport.clone(),
            self.router.clone(),
            self.directory.clone(),
            self.state_store.clone(),
            self.providers.clone(),
        );
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
}
