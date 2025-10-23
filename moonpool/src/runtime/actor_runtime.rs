//! Actor runtime - main entry point for actor system.

use crate::actor::{Actor, ActorFactory, ActorId, ActorRef, NodeId};
use crate::directory::SimpleDirectory;
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
pub struct ActorRuntime<T: TaskProvider> {
    /// Cluster namespace (all actors in this runtime share this namespace).
    namespace: String,

    /// This node's identifier.
    node_id: NodeId,

    /// Message bus for routing messages.
    message_bus: Rc<MessageBus>,

    /// Task provider for spawning actor message loops.
    task_provider: T,

    /// Router registry mapping actor type names to their catalogs.
    ///
    /// Maps actor type name (e.g., "BankAccount") → ActorCatalog<A, T, F>
    /// stored as trait object Rc<dyn ActorRouter>.
    ///
    /// This enables MessageBus to route messages to the correct catalog
    /// without knowing the concrete actor types at compile time.
    routers: RefCell<HashMap<String, Rc<dyn ActorRouter>>>,
}

impl<T: TaskProvider + 'static> ActorRuntime<T> {
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
    pub fn builder() -> ActorRuntimeBuilder<(), (), ()> {
        ActorRuntimeBuilder::new()
    }

    /// Create a new ActorRuntime (internal, used by builder).
    pub(crate) fn new(
        namespace: String,
        node_id: NodeId,
        message_bus: Rc<MessageBus>,
        task_provider: T,
    ) -> std::result::Result<Self, ActorError> {
        Ok(Self {
            namespace,
            node_id,
            message_bus,
            task_provider,
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

        // Create catalog for this actor type
        let catalog = Rc::new(ActorCatalog::new(
            self.node_id.clone(),
            self.task_provider.clone(),
            factory,
        ));

        // Set message bus on catalog
        catalog.set_message_bus(self.message_bus.clone());

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
