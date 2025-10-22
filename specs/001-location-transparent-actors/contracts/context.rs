// API Contract: ActorContext for Actor-to-Actor Communication
// This is a specification file, not compilable code

/// Context passed to actor methods for obtaining references to other actors.
///
/// ## Purpose
/// Provides actors with the ability to obtain references to other actors without
/// storing runtime state in the actor struct. This keeps actors lightweight and
/// makes them easier to test.
///
/// ## Design
/// Passed as a parameter to actor methods that need it, following the pattern:
/// ```ignore
/// pub async fn my_method(&mut self, ctx: &ActorContext, ...) -> Result<T, ActorError>
/// ```
///
/// ## Example: Actor-to-Actor Communication
/// ```ignore
/// impl BankAccountActor {
///     /// Transfer money to another account
///     pub async fn transfer_to(
///         &mut self,
///         ctx: &ActorContext,
///         recipient_key: &str,
///         amount: u64,
///     ) -> Result<(), ActorError> {
///         // Withdraw from self
///         if self.balance < amount {
///             return Err(ActorError::InsufficientFunds {
///                 available: self.balance,
///                 requested: amount,
///             });
///         }
///         self.balance -= amount;
///
///         // Get reference to recipient via context
///         let recipient: ActorRef<BankAccountActor> = ctx.get_actor("BankAccount", recipient_key);
///
///         // Call recipient
///         recipient.call(DepositRequest { amount }).await?;
///
///         Ok(())
///     }
/// }
/// ```
///
/// ## Usage from Client Code
/// ```ignore
/// let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice");
///
/// // The framework automatically passes ActorContext when dispatching messages
/// alice.call(TransferRequest {
///     recipient_key: "bob".to_string(),
///     amount: 50,
/// }).await?;
/// ```
///
/// ## Testing
/// ```ignore
/// // In tests, you can create a mock context
/// let mock_ctx = MockActorContext::new();
/// mock_ctx.register_actor("BankAccount", "bob", mock_recipient);
///
/// let mut actor = BankAccountActor::new(actor_id);
/// actor.transfer_to(&mock_ctx, "bob", 50).await?;
/// ```
pub struct ActorContext {
    namespace: String,
    message_bus: Arc<dyn MessageBus>,
}

impl ActorContext {
    /// Internal constructor used by the framework.
    pub(crate) fn new(namespace: String, message_bus: Arc<dyn MessageBus>) -> Self {
        Self {
            namespace,
            message_bus,
        }
    }

    /// Get a reference to an actor by type and key.
    ///
    /// ## Behavior
    /// - Returns immediately (does not activate actor)
    /// - Reference is valid regardless of activation state
    /// - Namespace is automatically applied from the runtime configuration
    /// - Type-safe: compiler enforces correct actor type
    ///
    /// ## Parameters
    /// - `actor_type` - The type identifier for the actor (e.g., "BankAccount")
    /// - `key` - The unique key within the actor type (e.g., "alice")
    ///
    /// ## Example
    /// ```ignore
    /// // Inside an actor method
    /// pub async fn notify_friend(&mut self, ctx: &ActorContext, friend_key: &str) -> Result<(), ActorError> {
    ///     let friend: ActorRef<UserActor> = ctx.get_actor("User", friend_key);
    ///     friend.send(NotificationMessage { ... }).await?;
    ///     Ok(())
    /// }
    /// ```
    pub fn get_actor<A: Actor>(
        &self,
        actor_type: impl Into<String>,
        key: impl Into<String>,
    ) -> ActorRef<A> {
        let actor_id = ActorId::new(
            self.namespace.clone(),
            actor_type.into(),
            key.into(),
        );

        ActorRef::new(actor_id, self.message_bus.clone())
    }

    /// Get the namespace for this context.
    ///
    /// Useful for logging or debugging.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
}

/// How ActorContext is passed to actor methods during message dispatch.
///
/// ## Framework Internals
///
/// When a message arrives for an actor, the framework:
///
/// 1. Looks up the actor's MessageHandler implementation
/// 2. Creates an ActorContext from the runtime state
/// 3. Passes the context to the handler's `handle()` method
/// 4. The handler calls the actor method with the context
///
/// ## Message Handler Pattern
/// ```ignore
/// #[async_trait(?Send)]
/// impl MessageHandler<TransferRequest, ()> for BankAccountActor {
///     async fn handle(&mut self, ctx: &ActorContext, req: TransferRequest) -> Result<(), ActorError> {
///         // Framework passes ctx here
///         self.transfer_to(ctx, &req.recipient_key, req.amount).await
///     }
/// }
/// ```
///
/// ## Note on MessageHandler Trait
/// The `MessageHandler` trait needs to be updated to include `ActorContext`:
///
/// ```ignore
/// #[async_trait(?Send)]
/// pub trait MessageHandler<Req, Res>: Actor {
///     async fn handle(&mut self, ctx: &ActorContext, request: Req) -> Result<Res, ActorError>;
///     //                         ^^^^^^^^^^^^^^^^^^^
///     //                         Context parameter added
/// }
/// ```
///
/// ## Benefits of Passing as Parameter
///
/// 1. **No Storage Overhead**: Actors don't need to store runtime references
/// 2. **Explicit Dependencies**: Clear which methods need actor references
/// 3. **Testability**: Easy to pass mock contexts in unit tests
/// 4. **Flexibility**: Different methods can use different contexts (e.g., for testing)
/// 5. **No Lifecycle Issues**: No need to initialize context in constructors
///
/// ## Alternative Patterns (Not Recommended)
///
/// ### Storing in Actor Struct
/// ```ignore
/// // ❌ Not recommended - adds storage overhead
/// pub struct BankAccountActor {
///     actor_id: ActorId,
///     ctx: ActorContext,  // Stored in every actor instance
///     balance: u64,
/// }
/// ```
///
/// ### Using Thread-Local
/// ```ignore
/// // ❌ Not recommended - implicit dependencies, hard to test
/// thread_local! {
///     static CONTEXT: RefCell<Option<ActorContext>> = RefCell::new(None);
/// }
/// ```
///
/// ## When to Use Each Pattern
///
/// ### Pass `&ActorContext` (Recommended)
/// Use when the actor needs to obtain references to other actors dynamically:
/// ```ignore
/// pub async fn process(&mut self, ctx: &ActorContext, user_ids: Vec<String>) -> Result<(), ActorError> {
///     for user_id in user_ids {
///         let user = ctx.get_actor("User", &user_id);
///         user.call(ProcessRequest).await?;
///     }
///     Ok(())
/// }
/// ```
///
/// ### Pass `ActorRef<A>` Directly
/// Use when the caller already has the reference and the actor doesn't need dynamic lookup:
/// ```ignore
/// pub async fn notify(&mut self, recipient: ActorRef<UserActor>, message: String) -> Result<(), ActorError> {
///     recipient.call(NotificationRequest { message }).await?;
///     Ok(())
/// }
/// ```
///
/// ### Store ActorRef in Actor State
/// Use when an actor consistently interacts with the same set of actors:
/// ```ignore
/// pub struct OrderActor {
///     order_id: String,
///     payment_service: ActorRef<PaymentServiceActor>,  // Set during activation
///     inventory_service: ActorRef<InventoryServiceActor>,
/// }
/// ```
