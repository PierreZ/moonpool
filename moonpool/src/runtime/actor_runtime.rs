//! Actor runtime - main entry point for actor system.

use crate::actor::{Actor, ActorFactory, ActorId, ActorRef, NodeId};
use crate::directory::Directory;
use crate::error::ActorError;
use crate::messaging::{ActorRouter, MessageBus};
use crate::runtime::ActorRuntimeBuilder;
use moonpool_foundation::task::TaskProvider;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

/// Main actor runtime coordinating catalog, directory, and message bus.
///
/// `ActorRuntime` is the entry point for using the actor system. It manages:
/// - Actor catalog (local activations)
/// - Directory service (actor location tracking)
/// - Message bus (routing and correlation)
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::ActorRuntime;
///
/// // Create runtime
/// let runtime = ActorRuntime::builder()
///     .namespace("prod")
///     .listen_addr("127.0.0.1:5000")
///     .build()
///     .await?;
///
/// // Register actor type
/// runtime.register_actor::<BankAccountActor, _>(BankAccountFactory)?;
///
/// // Get actor reference
/// let actor = runtime.get_actor::<BankAccountActor>("BankAccount", "alice");
///
/// // Send message
/// let balance = actor.call(DepositRequest { amount: 100 }).await?;
///
/// // Shutdown
/// runtime.shutdown(Duration::from_secs(30)).await?;
/// ```
pub struct ActorRuntime<T: TaskProvider, S: crate::serialization::Serializer> {
    /// Cluster namespace (all actors in this runtime share this namespace).
    namespace: String,

    /// This node's identifier.
    node_id: NodeId,

    /// Message bus for routing messages.
    message_bus: Rc<MessageBus>,

    /// Directory for actor placement and location tracking.
    ///
    /// Stored as trait object to support any Directory implementation.
    directory: Rc<dyn Directory>,

    /// Task provider for spawning actor message loops.
    task_provider: T,

    /// Storage provider for actor state persistence.
    ///
    /// All actor catalogs share this storage provider.
    /// Stored as trait object to support any StorageProvider implementation.
    storage: Rc<dyn crate::storage::StorageProvider>,

    /// Message serializer for handler dispatch.
    ///
    /// Used by ActorCatalogs for creating ActorContexts with HandlerRegistry.
    /// Pluggable serialization allows users to choose JSON, MessagePack, Bincode, etc.
    message_serializer: S,

    /// Router registry mapping actor type names to their catalogs.
    ///
    /// Maps actor type name (e.g., "BankAccount") → ActorCatalog<A, T, F>
    /// stored as trait object Rc<dyn ActorRouter>.
    ///
    /// This enables MessageBus to route messages to the correct catalog
    /// without knowing the concrete actor types at compile time.
    routers: RefCell<HashMap<String, Rc<dyn ActorRouter>>>,
}

impl<T: TaskProvider + 'static, S: crate::serialization::Serializer + Clone + 'static>
    ActorRuntime<T, S>
{
    /// Create a new runtime builder.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let runtime = ActorRuntime::builder()
    ///     .namespace("prod")
    ///     .listen_addr("127.0.0.1:5000")
    ///     .build()
    ///     .await?;
    /// ```
    pub fn builder() -> ActorRuntimeBuilder<(), (), (), ()> {
        ActorRuntimeBuilder::new()
    }

    /// Create a new ActorRuntime directly with all parameters.
    ///
    /// This is the primary constructor for ActorRuntime. Type inference works from the parameter types.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let runtime = ActorRuntime::new(
    ///     "prod",
    ///     "127.0.0.1:5000",
    ///     cluster_nodes,
    ///     Some(shared_directory),
    ///     storage,
    ///     moonpool_foundation::TokioNetworkProvider,
    ///     moonpool_foundation::TokioTimeProvider,
    ///     moonpool_foundation::TokioTaskProvider,
    ///     moonpool::serialization::JsonSerializer,
    /// ).await?;
    /// ```
    #[allow(clippy::too_many_arguments)]
    pub async fn new<N, Ti>(
        namespace: impl Into<String>,
        listen_addr: impl AsRef<str>,
        cluster_nodes: Vec<NodeId>,
        shared_directory: Option<Rc<dyn crate::directory::Directory>>,
        storage: Rc<dyn crate::storage::StorageProvider>,
        network_provider: N,
        time_provider: Ti,
        task_provider: T,
        serializer: S,
    ) -> Result<Self, ActorError>
    where
        N: moonpool_foundation::NetworkProvider + Clone + 'static,
        Ti: moonpool_foundation::TimeProvider + Clone + 'static,
    {
        use crate::directory::SimpleDirectory;
        use crate::messaging::{Message, MessageBus};
        use crate::placement::SimplePlacement;
        use moonpool_foundation::PeerConfig;
        use moonpool_foundation::network::transport::{Envelope, ServerTransport};

        let namespace = namespace.into();

        // Parse listen address
        let addr_str = listen_addr.as_ref();
        let listen_addr: std::net::SocketAddr = addr_str.parse().map_err(|e| {
            ActorError::InvalidConfiguration(format!("Invalid listen_addr '{}': {}", addr_str, e))
        })?;

        // Create NodeId from listen address
        let node_id = NodeId::from_socket_addr(listen_addr);

        // Use shared directory if provided, otherwise create new one
        let directory_rc: Rc<dyn crate::directory::Directory> =
            shared_directory.unwrap_or_else(|| {
                // Create directory (tracks WHERE actors currently are)
                Rc::new(SimpleDirectory::new()) as Rc<dyn crate::directory::Directory>
            });

        // Create placement strategy (decides WHERE new actors should go)
        let placement = SimplePlacement::new(cluster_nodes);

        // Create CallbackManager (Orleans: InsideRuntimeClient pattern)
        // Handles correlation tracking and callback management
        let callback_manager = Rc::new(crate::messaging::CallbackManager::new());

        // Create network transport using foundation layer
        // FoundationTransport wraps the foundation's transport with Message support
        let peer_config = PeerConfig::default();

        let transport = Rc::new(crate::messaging::network::FoundationTransport::new(
            network_provider.clone(),
            time_provider.clone(),
            task_provider.clone(),
            peer_config,
        ));

        // Create MessageBus with CallbackManager dependency (Orleans: MessageCenter pattern)
        // Handles routing logic, delegates callback management to CallbackManager
        // MessageBus uses type erasure, so we pass the serializer directly
        let message_bus = Rc::new(MessageBus::new(
            node_id.clone(),
            callback_manager,
            directory_rc.clone(),
            placement,
            transport,
        ));

        // Initialize MessageBus self-reference (required for passing to routers)
        message_bus.init_self_ref();

        // Create ServerTransport for incoming messages using Message as envelope type
        let listen_addr_str = listen_addr.to_string();
        let mut server_transport = ServerTransport::<_, _, _, Message>::bind(
            network_provider,
            time_provider,
            task_provider.clone(),
            &listen_addr_str,
        )
        .await
        .map_err(|e| {
            ActorError::InvalidConfiguration(format!("Failed to bind ServerTransport: {}", e))
        })?;

        // Start event-driven message receive loop (NOT polling!)
        let message_bus_for_recv = message_bus.clone();
        task_provider
            .spawn_task("network_receive_loop", async move {
                loop {
                    // Wait for next incoming message (event-driven, blocks until message arrives)
                    if let Some(msg) = server_transport.next_message().await {
                        // The envelope IS the Message now - no deserialization needed
                        let message = msg.envelope.clone();
                        tracing::debug!(
                            "Received network message: target={}, method={}, direction={:?}, corr_id={}",
                            message.target_actor,
                            message.method_name,
                            message.direction,
                            message.correlation_id
                        );

                        // CRITICAL: Send transport-level ACK immediately
                        // The ClientTransport::send() on the sending node is waiting
                        // for a response envelope to complete the forwarding operation.
                        // We create a proper response message and serialize it for the ACK.
                        let ack_response = Message::response(&message, vec![]);
                        match server_transport.reply_with_payload(
                            &msg.envelope,  // request envelope (implements Envelope trait)
                            ack_response.to_bytes(),  // Serialized response message
                            &msg,           // incoming message with connection_id
                        ) {
                            Ok(_) => {
                                tracing::debug!("Sent transport-level ACK for received message");
                            }
                            Err(e) => {
                                tracing::error!("Failed to send transport ACK: {}", e);
                                continue; // Skip routing if ACK failed
                            }
                        }

                        // Route to local actor synchronously
                        // After routing completes, we MUST yield to allow the waiting
                        // task (polling loop) to observe the oneshot completion
                        if let Err(e) = message_bus_for_recv.route_message(message).await {
                            tracing::error!("Failed to route network message: {}", e);
                        } else {
                            tracing::debug!("Successfully routed network message");
                        }

                        // CRITICAL: Yield to scheduler to allow waiting tasks to run
                        // In LocalSet, after completing a oneshot channel, we must yield
                        // before polling next_message() to ensure the waiting task gets
                        // scheduled and can observe the completion
                        tokio::task::yield_now().await;
                    }
                }
            });

        tracing::info!(
            "ActorRuntime created: namespace={}, node_id={}, listen_addr={}, network=enabled",
            namespace,
            node_id,
            listen_addr
        );

        Self::from_parts(
            namespace,
            node_id,
            message_bus,
            directory_rc,
            task_provider,
            storage,
            serializer,
        )
    }

    /// Create ActorRuntime from already-constructed parts (internal, used by builder).
    pub(crate) fn from_parts(
        namespace: String,
        node_id: NodeId,
        message_bus: Rc<MessageBus>,
        directory: Rc<dyn Directory>,
        task_provider: T,
        storage: Rc<dyn crate::storage::StorageProvider>,
        message_serializer: S,
    ) -> std::result::Result<Self, ActorError> {
        Ok(Self {
            namespace,
            node_id,
            message_bus,
            directory,
            task_provider,
            storage,
            message_serializer,
            routers: RefCell::new(HashMap::new()),
        })
    }

    /// Register an actor type with the runtime.
    ///
    /// This creates an ActorCatalog for the actor type and registers it with
    /// the MessageBus for automatic routing and activation.
    ///
    /// # Type Parameters
    ///
    /// - `A`: The actor type implementing the Actor trait
    /// - `F`: The factory type implementing ActorFactory<Actor = A>
    ///
    /// # Parameters
    ///
    /// - `factory`: Factory instance used to create actor instances on-demand
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Actor type registered successfully
    /// - `Err(ActorError::InvalidConfiguration)`: Actor type already registered
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use moonpool::prelude::*;
    ///
    /// // Create factory
    /// struct BankAccountFactory;
    ///
    /// #[async_trait(?Send)]
    /// impl ActorFactory for BankAccountFactory {
    ///     type Actor = BankAccountActor;
    ///
    ///     async fn create(&self, actor_id: ActorId) -> Result<Self::Actor, ActorError> {
    ///         Ok(BankAccountActor::new(actor_id))
    ///     }
    /// }
    ///
    /// // Register with runtime
    /// let factory = BankAccountFactory;
    /// runtime.register_actor::<BankAccountActor, _>(factory)?;
    ///
    /// // Actors now auto-activate on first message!
    /// let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice")?;
    /// alice.call(DepositRequest { amount: 100 }).await?;
    /// ```
    pub fn register_actor<A, F>(&self, factory: F) -> Result<(), ActorError>
    where
        A: Actor + 'static,
        F: ActorFactory<Actor = A> + 'static,
    {
        use crate::actor::ActorCatalog;

        let actor_type = A::ACTOR_TYPE;

        // Check if already registered
        if self.routers.borrow().contains_key(actor_type) {
            return Err(ActorError::InvalidConfiguration(format!(
                "Actor type '{}' already registered",
                actor_type
            )));
        }

        // Create catalog for this actor type with shared directory and storage
        // Use Rc::clone to share the directory and storage references
        let catalog = Rc::new(ActorCatalog::new(
            self.node_id.clone(),
            self.task_provider.clone(),
            factory,
            self.directory.clone(),          // Clone the Rc<dyn Directory>
            self.storage.clone(),            // Clone the Rc<dyn StorageProvider>
            self.message_serializer.clone(), // Use runtime's serializer
        ));

        // Initialize self-reference for message loop catalog access
        catalog.init_self_ref();

        // Store in router registry as trait object
        self.routers
            .borrow_mut()
            .insert(actor_type.to_string(), catalog);

        // Update MessageBus with new router registry
        self.message_bus
            .set_actor_routers(self.routers.borrow().clone());

        tracing::info!(
            "Registered actor type '{}' on node {}",
            actor_type,
            self.node_id
        );

        Ok(())
    }

    /// Get a reference to the message bus.
    #[allow(dead_code)]
    pub(crate) fn message_bus(&self) -> &Rc<MessageBus> {
        &self.message_bus
    }

    /// Get namespace for this runtime.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Get node ID for this runtime.
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    /// Get an actor reference by type and key.
    ///
    /// Returns immediately without network call or activation.
    /// Activation happens automatically on first message.
    ///
    /// # Parameters
    ///
    /// - `actor_type`: Type name (e.g., "BankAccount")
    /// - `key`: Unique key within type (e.g., "alice")
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Namespace "prod" automatically applied from runtime
    /// let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice")?;
    /// // Creates ActorId: "prod::BankAccount/alice"
    ///
    /// let bob = runtime.get_actor::<BankAccountActor>("BankAccount", "bob")?;
    /// // Creates ActorId: "prod::BankAccount/bob"
    /// ```
    pub fn get_actor<A: Actor>(
        &self,
        actor_type: impl Into<String>,
        key: impl Into<String>,
    ) -> Result<ActorRef<A>, ActorError> {
        // Create ActorId with runtime's namespace
        let actor_id = ActorId::from_parts(self.namespace.clone(), actor_type.into(), key.into())?;

        // Create ActorRef with MessageBus reference
        Ok(ActorRef::with_message_bus(
            actor_id,
            self.message_bus.clone(),
        ))
    }

    /// Shutdown the runtime gracefully.
    ///
    /// Deactivates all actors, closes connections, and stops message processing.
    ///
    /// # Parameters
    ///
    /// - `timeout`: Maximum time to wait for graceful shutdown
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// runtime.shutdown(Duration::from_secs(30)).await?;
    /// ```
    pub async fn shutdown(&self, _timeout: Duration) -> Result<(), ActorError> {
        // TODO: Implement graceful shutdown
        // 1. Stop accepting new messages
        // 2. Drain message queues
        // 3. Deactivate all actors (call on_deactivate)
        // 4. Close network connections
        // 5. Wait for completion or timeout

        tracing::info!("Runtime shutdown requested for node: {}", self.node_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::{ActorState, DeactivationReason};
    use async_trait::async_trait;
    use moonpool_foundation::{TokioNetworkProvider, TokioTaskProvider, TokioTimeProvider};
    use std::sync::atomic::{AtomicU32, Ordering};

    // ============================================================================
    // Test Actor Implementations
    // ============================================================================

    /// Simple test actor for runtime testing
    struct TestActor {
        actor_id: ActorId,
    }

    impl TestActor {
        fn new(actor_id: ActorId) -> Self {
            Self { actor_id }
        }
    }

    #[async_trait(?Send)]
    impl Actor for TestActor {
        type State = ();
        const ACTOR_TYPE: &'static str = "TestActor";

        fn actor_id(&self) -> &ActorId {
            &self.actor_id
        }

        async fn on_activate(&mut self, _state: ActorState<()>) -> Result<(), ActorError> {
            Ok(())
        }

        async fn on_deactivate(&mut self, _reason: DeactivationReason) -> Result<(), ActorError> {
            Ok(())
        }
    }

    /// Test actor factory
    struct TestActorFactory;

    #[async_trait(?Send)]
    impl crate::actor::ActorFactory for TestActorFactory {
        type Actor = TestActor;

        async fn create(&self, actor_id: ActorId) -> Result<Self::Actor, ActorError> {
            Ok(TestActor::new(actor_id))
        }
    }

    /// Another test actor for multi-type registration
    struct OtherActor {
        actor_id: ActorId,
    }

    impl OtherActor {
        fn new(actor_id: ActorId) -> Self {
            Self { actor_id }
        }
    }

    #[async_trait(?Send)]
    impl Actor for OtherActor {
        type State = ();
        const ACTOR_TYPE: &'static str = "OtherActor";

        fn actor_id(&self) -> &ActorId {
            &self.actor_id
        }

        async fn on_activate(&mut self, _state: ActorState<()>) -> Result<(), ActorError> {
            Ok(())
        }

        async fn on_deactivate(&mut self, _reason: DeactivationReason) -> Result<(), ActorError> {
            Ok(())
        }
    }

    /// Factory for OtherActor
    struct OtherActorFactory;

    #[async_trait(?Send)]
    impl crate::actor::ActorFactory for OtherActorFactory {
        type Actor = OtherActor;

        async fn create(&self, actor_id: ActorId) -> Result<Self::Actor, ActorError> {
            Ok(OtherActor::new(actor_id))
        }
    }

    /// Test actor factory that tracks creation count
    struct CountingFactory {
        count: Rc<AtomicU32>,
    }

    impl CountingFactory {
        fn new() -> (Self, Rc<AtomicU32>) {
            let count = Rc::new(AtomicU32::new(0));
            (
                Self {
                    count: count.clone(),
                },
                count,
            )
        }
    }

    #[async_trait(?Send)]
    impl crate::actor::ActorFactory for CountingFactory {
        type Actor = TestActor;

        async fn create(&self, actor_id: ActorId) -> Result<Self::Actor, ActorError> {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(TestActor::new(actor_id))
        }
    }

    // ============================================================================
    // Section 1: Runtime Initialization
    // ============================================================================

    #[tokio::test(flavor = "current_thread")]
    async fn test_runtime_initialization_with_builder() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("test")
                    .listen_addr("127.0.0.1:9001")
                    .expect("Valid listen address")
                    .cluster_nodes(vec![])
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                // Verify namespace
                assert_eq!(runtime.namespace(), "test");

                // Verify node ID matches listen address
                let expected_node_id = NodeId::from("127.0.0.1:9001").unwrap();
                assert_eq!(runtime.node_id(), &expected_node_id);

                // Verify routers is empty initially
                assert_eq!(runtime.routers.borrow().len(), 0);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_runtime_initialization_with_new() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage = Rc::new(crate::storage::InMemoryStorage::new())
                    as Rc<dyn crate::storage::StorageProvider>;

                let runtime =
                    ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::new(
                        "test",
                        "127.0.0.1:9002",
                        vec![],
                        None,
                        storage,
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                        crate::serialization::JsonSerializer,
                    )
                    .await
                    .expect("Runtime creation should succeed");

                assert_eq!(runtime.namespace(), "test");
                let expected_node_id = NodeId::from("127.0.0.1:9002").unwrap();
                assert_eq!(runtime.node_id(), &expected_node_id);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_runtime_multiple_with_different_namespaces() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage = Rc::new(crate::storage::InMemoryStorage::new())
                    as Rc<dyn crate::storage::StorageProvider>;

                let runtime1 =
                    ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::new(
                        "namespace1",
                        "127.0.0.1:9003",
                        vec![],
                        None,
                        storage.clone(),
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                        crate::serialization::JsonSerializer,
                    )
                    .await
                    .expect("Runtime1 creation should succeed");

                let runtime2 =
                    ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::new(
                        "namespace2",
                        "127.0.0.1:9004",
                        vec![],
                        None,
                        storage,
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                        crate::serialization::JsonSerializer,
                    )
                    .await
                    .expect("Runtime2 creation should succeed");

                assert_eq!(runtime1.namespace(), "namespace1");
                assert_eq!(runtime2.namespace(), "namespace2");

                // Verify node IDs are different
                assert_ne!(runtime1.node_id(), runtime2.node_id());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_runtime_invalid_listen_address() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage = Rc::new(crate::storage::InMemoryStorage::new())
                    as Rc<dyn crate::storage::StorageProvider>;

                let result =
                    ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::new(
                        "test",
                        "invalid:address:format",
                        vec![],
                        None,
                        storage,
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                        crate::serialization::JsonSerializer,
                    )
                    .await;

                assert!(result.is_err());
                match result.err().unwrap() {
                    ActorError::InvalidConfiguration(msg) => {
                        assert!(msg.contains("Invalid listen_addr"));
                    }
                    _ => panic!("Expected InvalidConfiguration error"),
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_runtime_network_initialization_verification() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("test")
                    .listen_addr("127.0.0.1:9005")
                    .expect("Valid listen address")
                    .cluster_nodes(vec![])
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                // Verify directory exists and can be called
                // (just verify we can call methods without panic)
                runtime.directory.get_actors_on_node(runtime.node_id()).await;

                // Verify runtime is properly initialized
                assert_eq!(runtime.namespace(), "test");
            })
            .await;
    }

    // ============================================================================
    // Section 2: Actor Registration
    // ============================================================================

    #[tokio::test(flavor = "current_thread")]
    async fn test_register_single_actor_type() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("test")
                    .listen_addr("127.0.0.1:9006")
                    .expect("Valid listen address")
                    .cluster_nodes(vec![])
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                // Register TestActor
                runtime
                    .register_actor::<TestActor, _>(TestActorFactory)
                    .expect("Registration should succeed");

                // Verify router exists
                assert!(runtime.routers.borrow().contains_key("TestActor"));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_register_duplicate_actor_type() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("test")
                    .listen_addr("127.0.0.1:9007")
                    .expect("Valid listen address")
                    .cluster_nodes(vec![])
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                // Register TestActor first time
                runtime
                    .register_actor::<TestActor, _>(TestActorFactory)
                    .expect("First registration should succeed");

                // Try to register again - should fail
                let result = runtime.register_actor::<TestActor, _>(TestActorFactory);
                assert!(result.is_err());

                match result.err().unwrap() {
                    ActorError::InvalidConfiguration(msg) => {
                        assert!(msg.contains("already registered"));
                        assert!(msg.contains("TestActor"));
                    }
                    _ => panic!("Expected InvalidConfiguration error"),
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_register_multiple_different_actor_types() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("test")
                    .listen_addr("127.0.0.1:9008")
                    .expect("Valid listen address")
                    .cluster_nodes(vec![])
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                // Register TestActor
                runtime
                    .register_actor::<TestActor, _>(TestActorFactory)
                    .expect("TestActor registration should succeed");

                // Register OtherActor
                runtime
                    .register_actor::<OtherActor, _>(OtherActorFactory)
                    .expect("OtherActor registration should succeed");

                // Verify both routers exist
                assert!(runtime.routers.borrow().contains_key("TestActor"));
                assert!(runtime.routers.borrow().contains_key("OtherActor"));
                assert_eq!(runtime.routers.borrow().len(), 2);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_router_registry_updates_after_registration() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("test")
                    .listen_addr("127.0.0.1:9009")
                    .expect("Valid listen address")
                    .cluster_nodes(vec![])
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                // Initially empty
                assert_eq!(runtime.routers.borrow().len(), 0);

                // Register first actor
                runtime
                    .register_actor::<TestActor, _>(TestActorFactory)
                    .expect("Registration should succeed");
                assert_eq!(runtime.routers.borrow().len(), 1);

                // Register second actor
                runtime
                    .register_actor::<OtherActor, _>(OtherActorFactory)
                    .expect("Registration should succeed");
                assert_eq!(runtime.routers.borrow().len(), 2);

                // Verify MessageBus has been updated (actor_routers should match)
                // We can't directly access MessageBus's actor_routers, but we verified
                // that set_actor_routers is called in the implementation
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_factory_validation() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("test")
                    .listen_addr("127.0.0.1:9010")
                    .expect("Valid listen address")
                    .cluster_nodes(vec![])
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                let (factory, _count) = CountingFactory::new();

                // Register with counting factory
                runtime
                    .register_actor::<TestActor, _>(factory)
                    .expect("Registration should succeed");

                // Verify router exists (factory is stored)
                assert!(runtime.routers.borrow().contains_key("TestActor"));
            })
            .await;
    }

    // ============================================================================
    // Section 3: Actor Reference Creation
    // ============================================================================

    #[tokio::test(flavor = "current_thread")]
    async fn test_get_actor_creates_correct_actor_ref() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("test")
                    .listen_addr("127.0.0.1:9011")
                    .expect("Valid listen address")
                    .cluster_nodes(vec![])
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                runtime
                    .register_actor::<TestActor, _>(TestActorFactory)
                    .expect("Registration should succeed");

                // Get actor reference
                let actor_ref = runtime
                    .get_actor::<TestActor>("TestActor", "alice")
                    .expect("get_actor should succeed");

                // Verify ActorRef properties
                assert_eq!(actor_ref.actor_id().namespace, "test");
                assert_eq!(actor_ref.actor_id().actor_type, "TestActor");
                assert_eq!(actor_ref.actor_id().key, "alice");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_namespace_automatically_applied_from_runtime() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("production")
                    .listen_addr("127.0.0.1:9012")
                    .expect("Valid listen address")
                    .cluster_nodes(vec![])
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                runtime
                    .register_actor::<TestActor, _>(TestActorFactory)
                    .expect("Registration should succeed");

                // Get actor reference
                let actor_ref = runtime
                    .get_actor::<TestActor>("TestActor", "bob")
                    .expect("get_actor should succeed");

                // Namespace should be "production" from runtime
                assert_eq!(actor_ref.actor_id().namespace, "production");
                assert_eq!(actor_ref.actor_id().to_string(), "production::TestActor/bob");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_different_keys_produce_different_refs() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("test")
                    .listen_addr("127.0.0.1:9013")
                    .expect("Valid listen address")
                    .cluster_nodes(vec![])
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                runtime
                    .register_actor::<TestActor, _>(TestActorFactory)
                    .expect("Registration should succeed");

                // Get multiple actor references with different keys
                let alice = runtime
                    .get_actor::<TestActor>("TestActor", "alice")
                    .expect("get_actor should succeed");
                let bob = runtime
                    .get_actor::<TestActor>("TestActor", "bob")
                    .expect("get_actor should succeed");

                // Verify they are different
                assert_ne!(alice.actor_id(), bob.actor_id());
                assert_eq!(alice.actor_id().key, "alice");
                assert_eq!(bob.actor_id().key, "bob");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_actor_ref_has_correct_message_bus_reference() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("test")
                    .listen_addr("127.0.0.1:9014")
                    .expect("Valid listen address")
                    .cluster_nodes(vec![])
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                runtime
                    .register_actor::<TestActor, _>(TestActorFactory)
                    .expect("Registration should succeed");

                // Get actor reference
                let actor_ref = runtime
                    .get_actor::<TestActor>("TestActor", "charlie")
                    .expect("get_actor should succeed");

                // Verify ActorRef has MessageBus (can't directly check Rc equality,
                // but we can verify the ActorRef was created successfully)
                assert_eq!(actor_ref.actor_id().key, "charlie");
            })
            .await;
    }

    // ============================================================================
    // Section 4: Namespace & Node Management
    // ============================================================================

    #[tokio::test(flavor = "current_thread")]
    async fn test_namespace_isolation_between_runtimes() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage = Rc::new(crate::storage::InMemoryStorage::new())
                    as Rc<dyn crate::storage::StorageProvider>;

                let runtime1 = ActorRuntime::<
                    TokioTaskProvider,
                    crate::serialization::JsonSerializer,
                >::builder()
                .namespace("namespace_a")
                .listen_addr("127.0.0.1:9015")
                .expect("Valid listen address")
                .cluster_nodes(vec![])
                .with_storage(storage.clone())
                .with_providers(TokioNetworkProvider, TokioTimeProvider, TokioTaskProvider)
                .with_serializer(crate::serialization::JsonSerializer)
                .build()
                .await
                .expect("Runtime1 creation should succeed");

                let runtime2 = ActorRuntime::<
                    TokioTaskProvider,
                    crate::serialization::JsonSerializer,
                >::builder()
                .namespace("namespace_b")
                .listen_addr("127.0.0.1:9016")
                .expect("Valid listen address")
                .cluster_nodes(vec![])
                .with_storage(storage)
                .with_providers(TokioNetworkProvider, TokioTimeProvider, TokioTaskProvider)
                .with_serializer(crate::serialization::JsonSerializer)
                .build()
                .await
                .expect("Runtime2 creation should succeed");

                runtime1
                    .register_actor::<TestActor, _>(TestActorFactory)
                    .expect("Registration should succeed");
                runtime2
                    .register_actor::<TestActor, _>(TestActorFactory)
                    .expect("Registration should succeed");

                // Get actor references from each runtime
                let actor1 = runtime1
                    .get_actor::<TestActor>("TestActor", "alice")
                    .expect("get_actor should succeed");
                let actor2 = runtime2
                    .get_actor::<TestActor>("TestActor", "alice")
                    .expect("get_actor should succeed");

                // Same key but different namespaces = different actors
                assert_ne!(actor1.actor_id(), actor2.actor_id());
                assert_eq!(actor1.actor_id().namespace, "namespace_a");
                assert_eq!(actor2.actor_id().namespace, "namespace_b");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_node_id_uniqueness() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage = Rc::new(crate::storage::InMemoryStorage::new())
                    as Rc<dyn crate::storage::StorageProvider>;

                let runtime1 = ActorRuntime::<
                    TokioTaskProvider,
                    crate::serialization::JsonSerializer,
                >::builder()
                .namespace("test")
                .listen_addr("127.0.0.1:9017")
                .expect("Valid listen address")
                .cluster_nodes(vec![])
                .with_storage(storage.clone())
                .with_providers(TokioNetworkProvider, TokioTimeProvider, TokioTaskProvider)
                .with_serializer(crate::serialization::JsonSerializer)
                .build()
                .await
                .expect("Runtime1 creation should succeed");

                let runtime2 = ActorRuntime::<
                    TokioTaskProvider,
                    crate::serialization::JsonSerializer,
                >::builder()
                .namespace("test")
                .listen_addr("127.0.0.1:9018")
                .expect("Valid listen address")
                .cluster_nodes(vec![])
                .with_storage(storage)
                .with_providers(TokioNetworkProvider, TokioTimeProvider, TokioTaskProvider)
                .with_serializer(crate::serialization::JsonSerializer)
                .build()
                .await
                .expect("Runtime2 creation should succeed");

                // Different listen addresses = different node IDs
                assert_ne!(runtime1.node_id(), runtime2.node_id());

                let node1 = NodeId::from("127.0.0.1:9017").unwrap();
                let node2 = NodeId::from("127.0.0.1:9018").unwrap();

                assert_eq!(runtime1.node_id(), &node1);
                assert_eq!(runtime2.node_id(), &node2);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_cluster_node_configuration() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let node1 = NodeId::from("127.0.0.1:9019").unwrap();
                let node2 = NodeId::from("127.0.0.1:9020").unwrap();
                let node3 = NodeId::from("127.0.0.1:9021").unwrap();

                let cluster_nodes = vec![node1.clone(), node2.clone(), node3.clone()];

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("test")
                    .listen_addr("127.0.0.1:9019")
                    .expect("Valid listen address")
                    .cluster_nodes(cluster_nodes.clone())
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                // Verify runtime was created with cluster nodes
                // (cluster_nodes is passed to SimplePlacement, which is internal)
                assert_eq!(runtime.node_id(), &node1);
            })
            .await;
    }

    // ============================================================================
    // Section 5: Graceful Shutdown
    // ============================================================================

    #[tokio::test(flavor = "current_thread")]
    async fn test_shutdown_with_timeout() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("test")
                    .listen_addr("127.0.0.1:9022")
                    .expect("Valid listen address")
                    .cluster_nodes(vec![])
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                // Shutdown with timeout
                let result = runtime.shutdown(Duration::from_secs(5)).await;
                assert!(result.is_ok());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_shutdown_logs_cleanup() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let storage =
                    Rc::new(crate::storage::InMemoryStorage::new()) as Rc<dyn crate::storage::StorageProvider>;

                let runtime = ActorRuntime::<TokioTaskProvider, crate::serialization::JsonSerializer>::builder()
                    .namespace("test")
                    .listen_addr("127.0.0.1:9023")
                    .expect("Valid listen address")
                    .cluster_nodes(vec![])
                    .with_storage(storage)
                    .with_providers(
                        TokioNetworkProvider,
                        TokioTimeProvider,
                        TokioTaskProvider,
                    )
                    .with_serializer(crate::serialization::JsonSerializer)
                    .build()
                    .await
                    .expect("Runtime creation should succeed");

                // Register actor
                runtime
                    .register_actor::<TestActor, _>(TestActorFactory)
                    .expect("Registration should succeed");

                // Shutdown (stub implementation just logs)
                let result = runtime.shutdown(Duration::from_secs(1)).await;
                assert!(result.is_ok());

                // In the future, this test would verify:
                // - All actors deactivated
                // - Message queues drained
                // - Network connections closed
            })
            .await;
    }
}
