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
    /// let account = runtime.get_actor::<BankAccountActor>("alice");
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

/// Actor runtime entry point.
///
/// Manages cluster-wide actor lifecycle and message routing.
pub struct ActorRuntime {
    namespace: String,
    node_id: NodeId,
    catalog: Arc<ActorCatalog>,
    directory: Arc<dyn Directory>,
    message_bus: Arc<dyn MessageBus>,
}

impl ActorRuntime {
    /// Create actor runtime for single-node cluster.
    ///
    /// ## Parameters
    /// - `namespace` - Cluster namespace (e.g., "prod", "staging", "tenant-acme")
    ///
    /// ## Example
    /// ```ignore
    /// // Production cluster
    /// let runtime = ActorRuntime::single_node("prod").await?;
    ///
    /// // Staging cluster (isolated from prod)
    /// let staging = ActorRuntime::single_node("staging").await?;
    ///
    /// // Multi-tenant: each tenant gets own cluster
    /// let acme_runtime = ActorRuntime::single_node("tenant-acme").await?;
    /// ```
    pub async fn single_node(namespace: impl Into<String>) -> Result<Self, ActorError> {
        unimplemented!("see runtime/mod.rs")
    }

    /// Create actor runtime for multi-node cluster.
    ///
    /// ## Parameters
    /// - `namespace` - Cluster namespace (all nodes must use same namespace)
    /// - `node_id` - This node's network address (host:port)
    /// - `peer_nodes` - List of peer node addresses in cluster (excluding this node)
    ///
    /// ## Example
    /// ```ignore
    /// // Node at 127.0.0.1:5000 joins production cluster
    /// let runtime = ActorRuntime::multi_node(
    ///     "prod",
    ///     NodeId::from("127.0.0.1:5000")?,
    ///     vec![
    ///         NodeId::from("127.0.0.1:5001")?,
    ///         NodeId::from("127.0.0.1:5002")?,
    ///     ],
    /// ).await?;
    ///
    /// // Node with hostname
    /// let runtime = ActorRuntime::multi_node(
    ///     "prod",
    ///     NodeId::from("node1.cluster:8080")?,
    ///     vec![
    ///         NodeId::from("node2.cluster:8080")?,
    ///         NodeId::from("node3.cluster:8080")?,
    ///     ],
    /// ).await?;
    /// ```
    pub async fn multi_node(
        namespace: impl Into<String>,
        node_id: NodeId,
        peer_nodes: Vec<NodeId>,
    ) -> Result<Self, ActorError> {
        unimplemented!("see runtime/mod.rs")
    }

    /// Obtain reference to actor by type and key.
    ///
    /// Namespace is automatically applied from ActorRuntime configuration.
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
    /// // Runtime bootstrapped with namespace "prod"
    /// let runtime = ActorRuntime::single_node("prod").await?;
    ///
    /// // get_actor automatically creates: ActorId { namespace: "prod", actor_type: "BankAccount", key: "alice" }
    /// let account: ActorRef<BankAccountActor> = runtime.get_actor("BankAccount", "alice");
    /// ```
    pub fn get_actor<A: Actor>(
        &self,
        actor_type: impl Into<String>,
        key: impl Into<String>,
    ) -> ActorRef<A> {
        // Internally creates: ActorId::new(self.namespace.clone(), actor_type, key)
        unimplemented!("see runtime/mod.rs")
    }

    /// Gracefully shutdown actor runtime.
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

/// Message handler trait for type-safe method dispatch.
///
/// **Manual Implementation Required**: Each actor must manually implement this trait
/// for each message type it handles. Future: proc macro for automatic derivation.
///
/// ## Pattern: Trait-Based Dispatch
/// Each (Actor, RequestType, ResponseType) triple gets a handler implementation.
///
/// ## Copy-Paste Template
/// ```ignore
/// // For each message type, copy-paste and fill in:
/// #[async_trait(?Send)]
/// impl MessageHandler<RequestType, ResponseType> for YourActor {
///     async fn handle(&mut self, req: RequestType) -> Result<ResponseType, ActorError> {
///         self.your_method(req.field).await
///     }
/// }
/// ```
///
/// ## Complete Example (BankAccountActor)
/// ```ignore
/// // Message types
/// #[derive(Serialize, Deserialize)]
/// pub struct DepositRequest { pub amount: u64 }
///
/// #[derive(Serialize, Deserialize)]
/// pub struct WithdrawRequest { pub amount: u64 }
///
/// #[derive(Serialize, Deserialize)]
/// pub struct GetBalanceRequest;
///
/// // Handler implementations (copy-paste pattern for each message)
/// #[async_trait(?Send)]
/// impl MessageHandler<DepositRequest, u64> for BankAccountActor {
///     async fn handle(&mut self, req: DepositRequest) -> Result<u64, ActorError> {
///         self.deposit(req.amount).await
///     }
/// }
///
/// #[async_trait(?Send)]
/// impl MessageHandler<WithdrawRequest, u64> for BankAccountActor {
///     async fn handle(&mut self, req: WithdrawRequest) -> Result<u64, ActorError> {
///         self.withdraw(req.amount).await
///     }
/// }
///
/// #[async_trait(?Send)]
/// impl MessageHandler<GetBalanceRequest, u64> for BankAccountActor {
///     async fn handle(&mut self, _req: GetBalanceRequest) -> Result<u64, ActorError> {
///         self.get_balance().await
///     }
/// }
/// ```
///
/// ## MessageBus Integration
/// When MessageBus receives a message for an actor:
/// 1. Deserialize `Message.payload` to concrete request type (e.g., `DepositRequest`)
/// 2. Look up handler via trait dispatch: `<BankAccountActor as MessageHandler<DepositRequest, u64>>::handle()`
/// 3. Execute handler method
/// 4. Serialize response back to `Vec<u8>` for return message
///
/// ## Type Safety
/// Compile-time guarantees that:
/// - Request type matches handler input
/// - Response type matches handler output
/// - All actor methods have corresponding handlers
///
/// ## Future: Automatic Derivation
/// Planned proc macro for automatic implementation (design TBD).
#[async_trait(?Send)]
pub trait MessageHandler<Req, Res>: Actor {
    async fn handle(&mut self, request: Req) -> Result<Res, ActorError>;
}

/// Message method registry for dynamic dispatch.
///
/// Maps method names to handler functions for runtime routing.
///
/// ## Alternative Pattern: Enum-Based Dispatch
/// Instead of traits, use an enum to represent all possible messages:
///
/// ```ignore
/// #[derive(Serialize, Deserialize)]
/// pub enum BankAccountMessage {
///     Deposit { amount: u64 },
///     Withdraw { amount: u64 },
///     GetBalance,
/// }
///
/// impl BankAccountActor {
///     async fn handle_message(&mut self, msg: BankAccountMessage) -> Result<BankAccountResponse, ActorError> {
///         match msg {
///             BankAccountMessage::Deposit { amount } => {
///                 let balance = self.deposit(amount).await?;
///                 Ok(BankAccountResponse::Balance(balance))
///             }
///             BankAccountMessage::Withdraw { amount } => {
///                 let balance = self.withdraw(amount).await?;
///                 Ok(BankAccountResponse::Balance(balance))
///             }
///             BankAccountMessage::GetBalance => {
///                 let balance = self.get_balance().await?;
///                 Ok(BankAccountResponse::Balance(balance))
///             }
///         }
///     }
/// }
/// ```
///
/// ## Trade-offs
/// **Trait-based** (recommended):
/// - ✅ Type-safe at compile time
/// - ✅ Each request/response pair is distinct type
/// - ✅ Easy to add new messages without changing enum
/// - ❌ Requires trait implementation boilerplate
///
/// **Enum-based**:
/// - ✅ Centralized message definition
/// - ✅ Exhaustiveness checking via match
/// - ❌ Less type-safe (single response enum for all methods)
/// - ❌ Harder to extend (need to modify enum)
///
/// ## Decision
/// Use **trait-based dispatch** for initial implementation (aligns with Orleans model).
/// Enum-based can be added later as an alternative pattern for specific use cases.

// Note: MessageBus routes incoming messages to actor methods via MessageHandler trait.
// See data-model.md "Message Flow: End-to-End" for complete routing flow.
// Implementation details in messaging/protocol.rs and actor/catalog.rs.
