// API Contract: Actor Trait and Lifecycle Hooks
// This is a specification file, not compilable code

/// Core actor trait that all actors must implement.
///
/// Actors process messages sequentially with single-threaded guarantees.
/// The framework handles activation, deactivation, and message routing.
#[async_trait(?Send)]
pub trait Actor: Sized {
    /// Called when actor is activated (before processing first message).
    ///
    /// Use for initialization logic: loading state, establishing connections, etc.
    ///
    /// ## Error Handling
    /// If activation fails, actor transitions to Deactivating state and is removed.
    /// A 5-second delay is imposed before removal to prevent activation storms.
    ///
    /// ## Example
    /// ```ignore
    /// async fn on_activate(&mut self) -> Result<()> {
    ///     tracing::info!("Actor {} activating", self.id);
    ///     // Initialize resources
    ///     Ok(())
    /// }
    /// ```
    async fn on_activate(&mut self) -> Result<(), ActorError> {
        Ok(())
    }

    /// Called when actor is deactivated (after processing last message).
    ///
    /// Use for cleanup logic: persisting state, closing connections, releasing resources.
    ///
    /// ## Deactivation Triggers
    /// - Idle timeout (no messages for configured duration)
    /// - Explicit deactivation request
    /// - Activation failure
    /// - Node shutdown
    ///
    /// ## Error Handling
    /// Errors are logged but do not prevent deactivation.
    /// Actor will still transition to Invalid state.
    ///
    /// ## Example
    /// ```ignore
    /// async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<()> {
    ///     tracing::info!("Actor {} deactivating: {:?}", self.id, reason);
    ///     // Clean up resources
    ///     Ok(())
    /// }
    /// ```
    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError> {
        Ok(())
    }
}

/// Reason for actor deactivation.
///
/// Provides context for cleanup logic in `on_deactivate()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeactivationReason {
    /// Actor idle for configured timeout duration
    IdleTimeout,

    /// Explicit deactivation request from application
    Explicit,

    /// Activation failed during `on_activate()`
    ActivationFailed,

    /// Node shutting down gracefully
    NodeShutdown,

    /// Lost directory registration race to another node
    DirectoryRace,
}

/// Actor reference for sending messages.
///
/// Provides location-transparent communication: caller doesn't know which node hosts the actor.
///
/// ## Type Safety
/// Generic over actor type ensures type-safe method calls.
///
/// ## Activation
/// First message automatically triggers actor activation if not already active.
pub struct ActorRef<A: Actor> {
    actor_id: ActorId,
    message_bus: Arc<dyn MessageBus>,
    _phantom: PhantomData<A>,
}

impl<A: Actor> ActorRef<A> {
    /// Send request and wait for response with default timeout (30 seconds).
    ///
    /// ## Behavior
    /// - Activates actor if not already active
    /// - Routes message to correct node via directory lookup
    /// - Correlates response with request
    /// - Enforces timeout
    ///
    /// ## Errors
    /// - `ActorError::Timeout` - Response not received within timeout
    /// - `ActorError::Activation Failed` - Actor failed to activate
    /// - `ActorError::Processing` - Actor threw exception during processing
    /// - `ActorError::NodeUnavailable` - Target node unreachable
    ///
    /// ## Example
    /// ```ignore
    /// let account = actor_system.get_actor::<BankAccountActor>("alice");
    /// let balance = account.call(GetBalanceRequest).await?;
    /// ```
    pub async fn call<Req, Res>(&self, request: Req) -> Result<Res, ActorError>
    where
        Req: serde::Serialize,
        Res: serde::de::DeserializeOwned,
    {
        // Implementation delegated to MessageBus
        unimplemented!("see MessageBus::request()")
    }

    /// Send request with custom timeout.
    ///
    /// Same as `call()` but allows specifying timeout duration.
    ///
    /// ## Example
    /// ```ignore
    /// let response = account
    ///     .call_with_timeout(request, Duration::from_secs(5))
    ///     .await?;
    /// ```
    pub async fn call_with_timeout<Req, Res>(
        &self,
        request: Req,
        timeout: Duration,
    ) -> Result<Res, ActorError>
    where
        Req: serde::Serialize,
        Res: serde::de::DeserializeOwned,
    {
        unimplemented!("see MessageBus::request_with_timeout()")
    }

    /// Send one-way message (fire-and-forget, no response).
    ///
    /// ## Behavior
    /// - Does not wait for response
    /// - Does not create CallbackData
    /// - Completes immediately after queuing message
    ///
    /// ## Use Cases
    /// - Notifications
    /// - Commands where response not needed
    /// - Best-effort delivery scenarios
    ///
    /// ## Errors
    /// Only fails if message cannot be queued (overload protection).
    ///
    /// ## Example
    /// ```ignore
    /// account.send(DepositNotification { amount: 100 }).await?;
    /// ```
    pub async fn send<Msg>(&self, message: Msg) -> Result<(), ActorError>
    where
        Msg: serde::Serialize,
    {
        unimplemented!("see MessageBus::send_oneway()")
    }

    /// Get actor ID.
    pub fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }
}

/// Actor system entry point.
///
/// Manages cluster-wide actor lifecycle and message routing.
pub struct ActorSystem {
    namespace: String,
    node_id: NodeId,
    catalog: Arc<ActorCatalog>,
    directory: Arc<dyn Directory>,
    message_bus: Arc<dyn MessageBus>,
}

impl ActorSystem {
    /// Create actor system for single-node cluster.
    ///
    /// ## Parameters
    /// - `namespace` - Cluster namespace (e.g., "prod", "staging", "tenant-acme")
    ///
    /// ## Example
    /// ```ignore
    /// // Production cluster
    /// let system = ActorSystem::single_node("prod").await?;
    ///
    /// // Staging cluster (isolated from prod)
    /// let staging = ActorSystem::single_node("staging").await?;
    ///
    /// // Multi-tenant: each tenant gets own cluster
    /// let acme_system = ActorSystem::single_node("tenant-acme").await?;
    /// ```
    pub async fn single_node(namespace: impl Into<String>) -> Result<Self, ActorError> {
        unimplemented!("see runtime/mod.rs")
    }

    /// Create actor system for multi-node cluster.
    ///
    /// ## Parameters
    /// - `namespace` - Cluster namespace (all nodes must use same namespace)
    /// - `node_id` - This node's unique identifier
    /// - `cluster_nodes` - List of all node addresses in cluster
    ///
    /// ## Example
    /// ```ignore
    /// // All nodes in production cluster use "prod" namespace
    /// let system = ActorSystem::multi_node(
    ///     "prod",
    ///     NodeId(0),
    ///     vec!["127.0.0.1:5000", "127.0.0.1:5001"],
    /// ).await?;
    /// ```
    pub async fn multi_node(
        namespace: impl Into<String>,
        node_id: NodeId,
        cluster_nodes: Vec<String>,
    ) -> Result<Self, ActorError> {
        unimplemented!("see runtime/mod.rs")
    }

    /// Obtain reference to actor by type and key.
    ///
    /// Namespace is automatically applied from ActorSystem configuration.
    ///
    /// ## Behavior
    /// - Returns immediately (does not activate actor)
    /// - Reference valid regardless of activation state
    /// - First message triggers activation if needed
    /// - Namespace from bootstrap is automatically applied
    ///
    /// ## Type Parameter
    /// `A` must implement `Actor` trait
    ///
    /// ## Example
    /// ```ignore
    /// // System bootstrapped with namespace "prod"
    /// let system = ActorSystem::single_node("prod").await?;
    ///
    /// // get_actor automatically creates: ActorId { namespace: "prod", actor_type: "BankAccount", key: "alice" }
    /// let account: ActorRef<BankAccountActor> = system.get_actor("BankAccount", "alice");
    /// ```
    pub fn get_actor<A: Actor>(
        &self,
        actor_type: impl Into<String>,
        key: impl Into<String>,
    ) -> ActorRef<A> {
        // Internally creates: ActorId::new(self.namespace.clone(), actor_type, key)
        unimplemented!("see runtime/mod.rs")
    }

    /// Gracefully shutdown actor system.
    ///
    /// ## Behavior
    /// - Deactivates all actors (calls `on_deactivate()`)
    /// - Waits for in-flight messages to complete
    /// - Closes network connections
    /// - Blocks until shutdown complete or timeout
    ///
    /// ## Timeout
    /// If shutdown exceeds timeout, forcibly terminates remaining actors.
    pub async fn shutdown(self, timeout: Duration) -> Result<(), ActorError> {
        unimplemented!("see runtime/mod.rs")
    }
}

/// Example actor implementation.
///
/// This demonstrates the expected developer experience.
pub struct BankAccountActor {
    actor_id: ActorId,
    balance: u64,
}

impl BankAccountActor {
    /// Constructor receives ActorId from framework (includes namespace)
    pub fn new(actor_id: ActorId) -> Self {
        Self {
            actor_id,
            balance: 0,
        }
    }

    /// Deposit money into account.
    pub async fn deposit(&mut self, amount: u64) -> Result<u64, ActorError> {
        self.balance += amount;
        Ok(self.balance)
    }

    /// Withdraw money from account.
    pub async fn withdraw(&mut self, amount: u64) -> Result<u64, ActorError> {
        if self.balance < amount {
            return Err(ActorError::InsufficientFunds {
                available: self.balance,
                requested: amount,
            });
        }
        self.balance -= amount;
        Ok(self.balance)
    }

    /// Get current balance.
    pub async fn get_balance(&self) -> Result<u64, ActorError> {
        Ok(self.balance)
    }
}

#[async_trait(?Send)]
impl Actor for BankAccountActor {
    async fn on_activate(&mut self) -> Result<(), ActorError> {
        tracing::info!("BankAccount {} activated", self.actor_id);
        Ok(())
    }

    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError> {
        tracing::info!("BankAccount {} deactivated: {:?}", self.actor_id, reason);
        Ok(())
    }
}

// Note: Message routing from ActorRef methods to actor methods happens via
// serde serialization + dynamic dispatch based on method name or enum variant.
// Implementation details in messaging/protocol.rs.
