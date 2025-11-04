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
