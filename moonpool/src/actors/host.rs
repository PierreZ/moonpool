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

use crate::{JsonCodec, MessageCodec, NetTransport, Providers, RequestStream, TaskProvider, UID};

use super::router::ActorError;
use super::types::{ActorId, ActorMessage, ActorResponse, ActorType};
use super::{ActorDirectory, ActorRouter};

/// Context provided to actor methods during dispatch.
///
/// Gives the actor access to its own identity and a router for calling
/// other actors (grain-to-grain communication, Orleans pattern).
pub struct ActorContext<P: Providers, C: MessageCodec = JsonCodec> {
    /// The identity of the actor currently being invoked.
    pub id: ActorId,
    /// Router for calling other actors.
    pub router: Rc<ActorRouter<P, C>>,
}

/// Trait implemented by each actor type for method dispatch.
///
/// The host calls `dispatch()` after looking up the actor instance.
/// Actor state requires `Default` — no activation hooks, no lifecycle,
/// no persistence. An actor is created via `Default::default()` on first
/// message and lives forever.
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
        providers: P,
    ) {
        let actor_type = H::actor_type();
        let token = UID::new(actor_type.0, 0);
        let stream: RequestStream<ActorMessage, C> =
            transport.register_handler(token, router.codec().clone());

        providers.task().spawn_task(
            "actor_host_loop",
            actor_processing_loop::<H, P, C>(transport, router, directory, stream),
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
async fn actor_processing_loop<H: ActorHandler, P: Providers, C: MessageCodec>(
    transport: Rc<NetTransport<P>>,
    router: Rc<ActorRouter<P, C>>,
    directory: Rc<dyn ActorDirectory>,
    stream: RequestStream<ActorMessage, C>,
) {
    let mut actors: HashMap<String, H> = HashMap::new();
    let actor_type = H::actor_type();

    loop {
        let Some((actor_msg, reply)) = stream
            .recv_with_transport::<_, ActorResponse>(&transport)
            .await
        else {
            // Stream closed — shutdown
            break;
        };

        // Look up or create the actor instance
        let identity = actor_msg.target.identity.clone();
        if !actors.contains_key(&identity) {
            // Activate new actor via Default::default()
            actors.insert(identity.clone(), H::default());

            // Register in directory (best-effort, ignore race conditions)
            let actor_id = ActorId::new(actor_type, identity.clone());
            let endpoint =
                crate::Endpoint::new(transport.local_address().clone(), UID::new(actor_type.0, 0));
            let _ = directory.register(&actor_id, endpoint).await;
        }

        let actor = actors
            .get_mut(&identity)
            .expect("actor was just inserted or already exists");

        // Build context for this invocation
        let ctx = ActorContext {
            id: ActorId::new(actor_type, identity),
            router: router.clone(),
        };

        // Dispatch the method call
        let result = actor
            .dispatch(&ctx, actor_msg.method, &actor_msg.body)
            .await;

        // Send response
        let response = match result {
            Ok(body) => ActorResponse { body: Ok(body) },
            Err(e) => ActorResponse {
                body: Err(e.to_string()),
            },
        };

        reply.send(response);
    }
}

/// Server-side runtime for virtual actors.
///
/// Owns actor instances, spawns processing loops, and handles activation
/// and method dispatch internally. Register actor types with `register()`,
/// and the host spawns a processing task for each type.
///
/// # Example
///
/// ```rust,ignore
/// let host = ActorHost::new(transport.clone(), router.clone(), directory.clone());
/// host.register::<BankAccountImpl>()?;
///
/// // Now BankAccountImpl actors are automatically activated and dispatched
/// // when messages arrive at their type's token (index 0).
/// ```
pub struct ActorHost<P: Providers, C: MessageCodec = JsonCodec> {
    /// The transport for registering endpoints.
    transport: Rc<NetTransport<P>>,
    /// Router for actor-to-actor calls (passed to ActorContext).
    router: Rc<ActorRouter<P, C>>,
    /// Directory for registering activated actors.
    directory: Rc<dyn ActorDirectory>,
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
            providers,
        }
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
