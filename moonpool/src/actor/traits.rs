//! Actor trait definitions.
//!
//! This module defines the core traits for actor lifecycle and message handling.

use crate::actor::{ActorContext, ActorId, DeactivationReason, PlacementHint};
use crate::error::ActorError;
use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Core actor trait defining lifecycle hooks and identity.
///
/// Actors are the fundamental unit of computation in the moonpool system.
/// Each actor has a unique identity (ActorId) and can process messages.
///
/// # Lifecycle
///
/// ```text
/// [NotExists] → [Creating] → [Activating] → [Valid] → [Deactivating] → [Invalid]
/// ```
///
/// # State Management
///
/// Actors can optionally persist state via the `State` associated type:
/// - **Stateless actors**: Use `type State = ()` (default)
/// - **Stateful actors**: Use custom type implementing `Serialize + DeserializeOwned`
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::prelude::*;
///
/// pub struct MyActor {
///     actor_id: ActorId,
///     counter: u64,
/// }
///
/// #[async_trait(?Send)]
/// impl Actor for MyActor {
///     type State = ();  // Stateless
///
///     fn actor_id(&self) -> &ActorId {
///         &self.actor_id
///     }
///
///     async fn on_activate(&mut self, _state: Option<()>) -> Result<(), ActorError> {
///         tracing::info!("Actor activated: {}", self.actor_id);
///         Ok(())
///     }
///
///     async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError> {
///         tracing::info!("Actor deactivating: {:?}", reason);
///         Ok(())
///     }
/// }
/// ```
#[async_trait(?Send)]
pub trait Actor: Sized {
    /// Persistent state type.
    ///
    /// For stateless actors, use `type State = ()`.
    /// For stateful actors, use a custom type implementing `Serialize + DeserializeOwned`.
    /// The framework will automatically load this state on activation and pass it
    /// to `on_activate()`.
    type State: Serialize + DeserializeOwned;

    /// Actor type name for routing and catalog registration.
    ///
    /// This constant identifies the actor type across the cluster and is used by:
    /// - **ActorRuntime** to register catalogs for this actor type
    /// - **MessageBus** to route messages to the correct catalog
    /// - **Directory** to track actor placements by type
    ///
    /// # Naming Convention
    ///
    /// Use PascalCase matching the struct name (e.g., "BankAccount", "OrderProcessor").
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// impl Actor for BankAccountActor {
    ///     type State = BankAccountState;
    ///     const ACTOR_TYPE: &'static str = "BankAccount";
    ///     // ...
    /// }
    /// ```
    const ACTOR_TYPE: &'static str;

    /// Get this actor's unique identifier.
    fn actor_id(&self) -> &ActorId;

    /// Provide a placement hint for actor activation.
    ///
    /// This method influences where the directory places new actor activations.
    /// It's a **hint**, not a guarantee - the directory may ignore it based on
    /// cluster state (e.g., if the preferred node is unavailable).
    ///
    /// # Default Implementation
    ///
    /// Returns `PlacementHint::Random` for load-balanced distribution across nodes.
    ///
    /// # Override Examples
    ///
    /// **Local placement** (minimize network overhead):
    /// ```rust,ignore
    /// fn placement_hint() -> PlacementHint {
    ///     PlacementHint::Local  // Prefer caller's node
    /// }
    /// ```
    ///
    /// # When to Override
    ///
    /// - **High-throughput actors**: Use `Local` to reduce cross-node calls
    /// - **Session actors**: Use `Local` for strong locality
    /// - **Load-balanced actors**: Use `Random` (default) for even distribution
    ///
    /// # Relationship to Orleans
    ///
    /// This replaces Orleans' `[StatelessWorker]` and `[OneInstancePerCluster]` attributes
    /// with a simpler, more flexible hint-based system.
    fn placement_hint() -> PlacementHint {
        PlacementHint::Random
    }

    /// Called when actor is activated (before first message).
    ///
    /// # Parameters
    ///
    /// - `state`: Previously persisted state loaded by framework.
    ///   - `None`: First activation or no persisted state exists
    ///   - `Some(state)`: State loaded from storage
    ///
    /// # Timing
    ///
    /// - Called during the `Activating` state transition
    /// - Blocks message processing until completion
    /// - Transition to `Valid` state on success
    ///
    /// # Error Handling
    ///
    /// If this method returns an error:
    /// - Actor transitions to `Deactivating` state
    /// - `on_deactivate()` is called with `DeactivationReason::ActivationFailed`
    /// - Actor removed from catalog after 5-second delay (configurable)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// async fn on_activate(&mut self, state: Option<MyState>) -> Result<(), ActorError> {
    ///     let initial_state = state.unwrap_or_default();
    ///     self.state = ActorState::new_with_storage(
    ///         initial_state,
    ///         self.actor_id.clone(),
    ///         storage,
    ///         JsonSerializer,
    ///     );
    ///     tracing::info!("Activated with balance: {}", self.state.get().balance);
    ///     Ok(())
    /// }
    /// ```
    async fn on_activate(&mut self, state: Option<Self::State>) -> Result<(), ActorError>;

    /// Called when actor is deactivated (after last message).
    ///
    /// # Parameters
    ///
    /// - `reason`: Why deactivation was triggered
    ///
    /// # Timing
    ///
    /// - Called during the `Deactivating` state transition
    /// - No new messages will be processed
    /// - Actor transitions to `Invalid` state after completion
    ///
    /// # Error Handling
    ///
    /// Errors are logged but do not prevent deactivation from completing.
    /// Actor will be removed from catalog regardless of return value.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError> {
    ///     tracing::info!("Deactivating: {:?}, final balance: {}",
    ///                    reason, self.state.get().balance);
    ///     // Optional: Perform cleanup (close connections, flush logs, etc.)
    ///     Ok(())
    /// }
    /// ```
    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError>;

    /// Register message handlers for this actor type.
    ///
    /// Override this method to register handlers for each `MessageHandler` implementation.
    /// The framework calls this once during `ActorContext` initialization to build
    /// the handler registry for dynamic message dispatch.
    ///
    /// # Default Implementation
    ///
    /// The default implementation does nothing (no handlers registered).
    /// This is appropriate for system actors that don't handle user messages.
    ///
    /// # Handler Registration
    ///
    /// For each `MessageHandler<Req, Res>` trait implementation, call:
    /// ```rust,ignore
    /// registry.register::<RequestType, ResponseType>();
    /// ```
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use moonpool::prelude::*;
    ///
    /// struct BankAccountActor {
    ///     actor_id: ActorId,
    ///     balance: u64,
    /// }
    ///
    /// #[async_trait(?Send)]
    /// impl Actor for BankAccountActor {
    ///     type State = ();
    ///     const ACTOR_TYPE: &'static str = "BankAccount";
    ///
    ///     fn actor_id(&self) -> &ActorId {
    ///         &self.actor_id
    ///     }
    ///
    ///     async fn on_activate(&mut self, _state: Option<()>) -> Result<(), ActorError> {
    ///         Ok(())
    ///     }
    ///
    ///     async fn on_deactivate(&mut self, _reason: DeactivationReason) -> Result<(), ActorError> {
    ///         Ok(())
    ///     }
    ///
    ///     // Register all handlers
    ///     fn register_handlers(registry: &mut HandlerRegistry<Self>) {
    ///         registry.register::<DepositRequest, u64>();
    ///         registry.register::<WithdrawRequest, u64>();
    ///         registry.register::<GetBalanceRequest, u64>();
    ///     }
    /// }
    /// ```
    ///
    /// # Future: Derive Macro
    ///
    /// In the future, this can be auto-generated via `#[derive(ActorDispatch)]`:
    /// ```rust,ignore
    /// #[derive(ActorDispatch)]  // Scans for MessageHandler impls
    /// impl Actor for BankAccountActor {
    ///     // No need to write register_handlers() manually!
    /// }
    /// ```
    fn register_handlers(_registry: &mut crate::actor::HandlerRegistry<Self>) {
        // Default: no handlers (for system actors)
    }
}

/// Message handler trait for processing typed requests.
///
/// Actors implement this trait for each message type they can handle.
/// The framework automatically routes messages based on type name and
/// deserializes payloads before calling `handle()`.
///
/// # Type Parameters
///
/// - `Req`: Request message type (must implement `Serialize + DeserializeOwned`)
/// - `Res`: Response type (must implement `Serialize + DeserializeOwned`)
///
/// # Dispatch Mechanism
///
/// The framework matches messages to handlers using the request type name:
///
/// ```text
/// Message { method_name: "DepositRequest", payload: [...] }
///   ↓
/// <BankAccountActor as MessageHandler<DepositRequest, u64>>::handle(...)
/// ```
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::prelude::*;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Debug, Serialize, Deserialize)]
/// pub struct DepositRequest {
///     pub amount: u64,
/// }
///
/// #[async_trait(?Send)]
/// impl MessageHandler<DepositRequest, u64> for BankAccountActor {
///     async fn handle(
///         &mut self,
///         req: DepositRequest,
///         _ctx: &ActorContext,
///     ) -> Result<u64, ActorError> {
///         // Update state
///         self.balance += req.amount;
///
///         // Persist changes
///         self.state.persist(self.balance).await?;
///
///         Ok(self.balance)
///     }
/// }
/// ```
///
/// # Error Handling
///
/// If `handle()` returns an error:
/// - Error is serialized and sent back to caller
/// - Actor remains in `Valid` state (error isolation)
/// - Next message in queue is processed normally
///
/// # Context Access
///
/// The `ctx` parameter provides access to:
/// - Actor metadata (actor_id, node_id)
/// - State transitions (query current state)
/// - Actor-to-actor messaging (future: `ctx.get_actor()`)
#[async_trait(?Send)]
pub trait MessageHandler<Req, Res>: Actor
where
    Req: Serialize + DeserializeOwned,
    Res: Serialize + DeserializeOwned,
{
    /// Handle a typed request message.
    ///
    /// # Parameters
    ///
    /// - `req`: Deserialized request payload
    /// - `ctx`: Actor execution context (metadata, state access)
    ///
    /// # Returns
    ///
    /// - `Ok(response)`: Response will be serialized and sent to caller
    /// - `Err(error)`: Error will be propagated to caller as ActorError
    ///
    /// # Concurrency
    ///
    /// Messages are processed **sequentially** per actor. This method will
    /// never be called concurrently for the same actor instance, providing
    /// single-threaded execution guarantees.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// async fn handle(
    ///     &mut self,
    ///     req: WithdrawRequest,
    ///     _ctx: &ActorContext,
    /// ) -> Result<u64, ActorError> {
    ///     if req.amount > self.balance {
    ///         return Err(ActorError::InsufficientFunds {
    ///             requested: req.amount,
    ///             available: self.balance,
    ///         });
    ///     }
    ///
    ///     self.balance -= req.amount;
    ///     self.state.persist(self.balance).await?;
    ///
    ///     Ok(self.balance)
    /// }
    /// ```
    async fn handle(&mut self, req: Req, ctx: &ActorContext<Self>) -> Result<Res, ActorError>;
}
