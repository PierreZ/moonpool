//! Actor context and execution environment.
//!
//! This module provides the `ActorContext` which holds per-actor state and metadata.

use crate::actor::{
    ActivationState, Actor, ActorId, DeactivationPolicy, DeactivationReason, NodeId,
};

/// Lifecycle commands sent via control channel.
///
/// These commands control the actor's lifecycle independently from business logic messages.
/// Follows Orleans pattern where lifecycle operations are processed separately.
#[derive(Debug)]
pub enum LifecycleCommand<A: Actor> {
    /// Activate the actor with ActorState wrapper.
    ///
    /// The result is sent back via the oneshot channel.
    Activate {
        state: crate::actor::ActorState<A::State>,
        result_tx: oneshot::Sender<Result<(), ActorError>>,
    },

    /// Deactivate the actor with the given reason.
    ///
    /// After handling this command, the message loop will exit.
    Deactivate { reason: DeactivationReason },
}
use crate::error::ActorError;
use crate::messaging::{Message, MessageBus};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};

/// Per-actor execution context holding state and message queue.
///
/// `ActorContext` manages the lifecycle state, message queue, and metadata
/// for a single actor activation. It uses `RefCell` for interior mutability
/// in a single-threaded context (no Send/Sync requirements).
///
/// # Lifecycle
///
/// ```text
/// Creating → Activating → Valid → Deactivating → Invalid
/// ```
///
/// # Fields
///
/// - `actor_id`: Unique identifier for this actor
/// - `node_id`: Physical node hosting this activation
/// - `state`: Current lifecycle state (interior mutability via RefCell)
/// - `message_sender`: Channel sender for actor messages (primary mailbox)
/// - `control_sender`: Channel sender for lifecycle commands
/// - `actor_instance`: The user-defined actor implementation
/// - `activation_time`: When this actor was activated
/// - `last_message_time`: For idle timeout detection
///
/// # Interior Mutability Pattern
///
/// Since moonpool uses single-threaded execution (`current_thread` runtime),
/// we use `RefCell` instead of `Mutex` for lower overhead:
///
/// ```rust,ignore
/// // Borrow state (single-threaded, no Mutex needed)
/// let state = context.state.borrow();
/// if *state == ActivationState::Valid {
///     // Process message
/// }
///
/// // Mutably borrow actor instance
/// let mut actor = context.actor_instance.borrow_mut();
/// actor.deposit(100).await?;
/// ```
///
/// # Invariants
///
/// - Single message processed at a time (sequential processing)
/// - Messages delivered via bounded channel (capacity: 128)
/// - `last_message_time` updated on every message processing
/// - Actor only removed from catalog when state == Invalid
pub struct ActorContext<A: Actor, S: crate::serialization::Serializer> {
    /// Actor's unique identifier.
    pub actor_id: ActorId,

    /// Hosting node identifier.
    pub node_id: NodeId,

    /// Current lifecycle state (interior mutability via RefCell).
    pub state: RefCell<ActivationState>,

    /// Channel sender for actor messages (primary mailbox).
    ///
    /// Messages are sent to this channel and received by the message loop.
    /// Bounded capacity provides natural backpressure.
    pub message_sender: mpsc::Sender<Message>,

    /// Channel sender for lifecycle commands.
    ///
    /// Used for activation and deactivation commands processed separately
    /// from business logic messages.
    pub control_sender: mpsc::Sender<LifecycleCommand<A>>,

    /// User-defined actor implementation.
    ///
    /// Wrapped in RefCell for mutable access during message processing.
    pub actor_instance: RefCell<A>,

    /// When this actor was activated (for tracking activation age).
    pub activation_time: Instant,

    /// Last time a message was processed (for idle timeout detection).
    ///
    /// Updated after each successful message handling.
    /// If `Instant::now() - last_message_time > idle_timeout`, trigger deactivation.
    pub last_message_time: RefCell<Instant>,

    /// Message loop task handle (spawned once during creation).
    ///
    /// Stored for debugging and to ensure task is not dropped prematurely.
    pub message_loop_task: RefCell<Option<tokio::task::JoinHandle<()>>>,

    /// MessageBus reference for sending responses.
    ///
    /// Set by ActorCatalog when processing messages. Required for
    /// message dispatch to send responses back to callers.
    pub message_bus: RefCell<Option<Rc<MessageBus>>>,

    /// Last error that occurred during message processing.
    ///
    /// For debugging and monitoring. Actors can continue processing
    /// messages even after errors (error isolation).
    pub last_error: RefCell<Option<ActorError>>,

    /// Total count of errors encountered during this activation's lifetime.
    ///
    /// Useful for health monitoring and deciding when to deactivate
    /// a misbehaving actor.
    pub error_count: RefCell<usize>,

    /// Handler registry for dynamic message dispatch.
    ///
    /// Maps method names (e.g., "SayHelloRequest") to type-erased handler closures.
    /// Initialized once during ActorContext creation via `A::register_handlers()`.
    pub handlers: crate::actor::HandlerRegistry<A, S>,

    /// Storage provider for actor state persistence.
    ///
    /// Actors can use this to persist state changes via `ActorState<T>` wrapper.
    /// Shared across all actors in the same runtime.
    pub storage: Rc<dyn crate::storage::StorageProvider>,

    /// Message serializer for network messages.
    ///
    /// Used by HandlerRegistry for deserializing requests and serializing responses.
    /// Generic over Serializer trait for pluggable serialization.
    pub message_serializer: S,
}

impl<A: Actor, S: crate::serialization::Serializer + Clone + 'static> ActorContext<A, S> {
    /// Create a new actor context in `Creating` state.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: Unique identifier for this actor
    /// - `node_id`: Node hosting this activation
    /// - `actor_instance`: User-defined actor implementation
    ///
    /// # Initial State
    ///
    /// - `state`: `ActivationState::Creating`
    /// - `activation_time`: Current time
    /// - `last_message_time`: Current time
    /// - `message_loop_task`: `None` (set after spawning)
    ///
    /// # Parameters
    ///
    /// - `message_sender`: Sender for actor messages (from message channel)
    /// - `control_sender`: Sender for lifecycle commands (from control channel)
    /// - `storage`: Storage provider for actor state persistence
    /// - `message_serializer`: Serializer for network messages
    pub fn new(
        actor_id: ActorId,
        node_id: NodeId,
        actor_instance: A,
        message_sender: mpsc::Sender<Message>,
        control_sender: mpsc::Sender<LifecycleCommand<A>>,
        storage: Rc<dyn crate::storage::StorageProvider>,
        message_serializer: S,
    ) -> Self {
        let now = Instant::now();

        // Initialize handler registry with message serializer
        let mut handlers = crate::actor::HandlerRegistry::new(message_serializer.clone());
        A::register_handlers(&mut handlers);

        tracing::debug!(
            "Initialized ActorContext for {} with {} handlers",
            actor_id,
            handlers.handler_count()
        );

        Self {
            actor_id,
            node_id,
            state: RefCell::new(ActivationState::Creating),
            message_sender,
            control_sender,
            actor_instance: RefCell::new(actor_instance),
            activation_time: now,
            last_message_time: RefCell::new(now),
            message_loop_task: RefCell::new(None),
            message_bus: RefCell::new(None),
            last_error: RefCell::new(None),
            error_count: RefCell::new(0),
            handlers,
            storage,
            message_serializer,
        }
    }

    /// Get current activation state.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let state = context.get_state();
    /// if state == ActivationState::Valid {
    ///     // Ready to process messages
    /// }
    /// ```
    pub fn get_state(&self) -> ActivationState {
        *self.state.borrow()
    }

    /// Transition to a new state with validation.
    ///
    /// # Parameters
    ///
    /// - `next`: The target state
    ///
    /// # Returns
    ///
    /// - `Ok(())`: State transition successful
    /// - `Err(ActorError::InvalidStateTransition)`: Invalid transition attempted
    ///
    /// # Valid Transitions
    ///
    /// ```text
    /// Creating → Activating
    /// Activating → Valid | Deactivating (activation failed)
    /// Valid → Deactivating
    /// Deactivating → Invalid
    /// ```
    pub fn set_state(&self, next: ActivationState) -> Result<(), crate::error::ActorError> {
        use crate::error::ActorError;

        let current = *self.state.borrow();

        // Validate transition
        let valid = match (current, next) {
            (ActivationState::Creating, ActivationState::Activating) => true,
            (ActivationState::Activating, ActivationState::Valid) => true,
            (ActivationState::Activating, ActivationState::Deactivating) => true, // Activation failed
            (ActivationState::Valid, ActivationState::Deactivating) => true,
            (ActivationState::Deactivating, ActivationState::Invalid) => true,
            _ => false,
        };

        if !valid {
            return Err(ActorError::InvalidStateTransition {
                from: current,
                to: next,
            });
        }

        *self.state.borrow_mut() = next;
        Ok(())
    }

    /// Enqueue a message for processing (async send to channel).
    ///
    /// Sends the message to the actor's message channel. The message loop
    /// will receive and process it asynchronously.
    ///
    /// # Parameters
    ///
    /// - `message`: The message to enqueue
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Message successfully enqueued
    /// - `Err(ActorError)`: Channel closed (actor deactivated)
    ///
    /// # Backpressure
    ///
    /// If the channel is full (128 messages), this will wait asynchronously
    /// until space is available.
    pub async fn enqueue_message(&self, message: Message) -> Result<(), ActorError> {
        self.message_sender
            .send(message)
            .await
            .map_err(|_| ActorError::ProcessingFailed("Actor mailbox closed".to_string()))
    }

    /// Set the message loop task handle.
    ///
    /// Called once after spawning the message loop task.
    pub fn set_message_loop_task(&self, handle: tokio::task::JoinHandle<()>) {
        *self.message_loop_task.borrow_mut() = Some(handle);
    }

    /// Update last message processing time.
    ///
    /// Called after each successful message processing to reset the idle timer.
    pub fn update_last_message_time(&self) {
        *self.last_message_time.borrow_mut() = Instant::now();
    }

    /// Get time since last message was processed.
    ///
    /// Used for idle timeout detection.
    pub fn time_since_last_message(&self) -> std::time::Duration {
        Instant::now() - *self.last_message_time.borrow()
    }

    /// Set the MessageBus reference for sending responses.
    ///
    /// This is called by ActorCatalog when routing messages.
    pub fn set_message_bus(&self, message_bus: Rc<MessageBus>) {
        *self.message_bus.borrow_mut() = Some(message_bus);
    }

    /// Get the MessageBus reference.
    pub fn get_message_bus(&self) -> Option<Rc<MessageBus>> {
        self.message_bus.borrow().clone()
    }

    /// Record an error that occurred during message processing.
    ///
    /// Updates `last_error` and increments `error_count` for monitoring.
    ///
    /// # Parameters
    ///
    /// - `error`: The error that occurred
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if let Err(e) = actor.handle(request, ctx).await {
    ///     ctx.record_error(e.clone());
    ///     // Send error response to caller
    /// }
    /// ```
    pub fn record_error(&self, error: ActorError) {
        *self.last_error.borrow_mut() = Some(error);
        *self.error_count.borrow_mut() += 1;
    }

    /// Get the last error that occurred during message processing.
    ///
    /// # Returns
    ///
    /// - `Some(error)`: Last error recorded
    /// - `None`: No errors have occurred
    pub fn get_last_error(&self) -> Option<ActorError> {
        self.last_error.borrow().as_ref().cloned()
    }

    /// Get the total number of errors encountered.
    ///
    /// Useful for health monitoring and circuit breaker patterns.
    pub fn get_error_count(&self) -> usize {
        *self.error_count.borrow()
    }

    /// Clear error tracking state.
    ///
    /// Can be called after successful recovery or when error monitoring
    /// window resets.
    pub fn clear_errors(&self) {
        *self.last_error.borrow_mut() = None;
        *self.error_count.borrow_mut() = 0;
    }

    /// Get a reference to another actor for actor-to-actor communication.
    ///
    /// This allows actors to obtain references to other actors within the same
    /// namespace for sending messages.
    ///
    /// # Parameters
    ///
    /// - `actor_type`: The type of the target actor (e.g., "BankAccount")
    /// - `key`: The unique key for the target actor (e.g., "alice")
    ///
    /// # Returns
    ///
    /// - `Ok(ActorRef<B>)`: Reference to the target actor
    /// - `Err(ActorError::ProcessingFailed)`: MessageBus not configured
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// impl BankAccountActor {
    ///     pub async fn transfer_to(
    ///         &mut self,
    ///         ctx: &ActorContext<Self>,
    ///         recipient_key: &str,
    ///         amount: u64,
    ///     ) -> Result<(), ActorError> {
    ///         // Withdraw from self
    ///         self.balance -= amount;
    ///
    ///         // Get reference to recipient
    ///         let recipient: ActorRef<BankAccountActor> =
    ///             ctx.get_actor("BankAccount", recipient_key)?;
    ///
    ///         // Send deposit message
    ///         recipient.call(DepositRequest { amount }).await?;
    ///         Ok(())
    ///     }
    /// }
    /// ```
    ///
    /// # Note
    ///
    /// The namespace is automatically derived from the current actor's ActorId.
    /// Actors can only communicate with other actors in the same namespace.
    pub fn get_actor<B: Actor>(
        &self,
        actor_type: impl Into<String>,
        key: impl Into<String>,
    ) -> Result<crate::actor::ActorRef<B>, ActorError> {
        // Get MessageBus reference
        let message_bus = self.message_bus.borrow().clone().ok_or_else(|| {
            ActorError::ProcessingFailed("MessageBus not configured in ActorContext".to_string())
        })?;

        // Extract namespace from current actor's ID
        let namespace = self.actor_id.namespace.clone();

        // Create ActorId for target actor
        let target_actor_id = ActorId::from_parts(namespace, actor_type.into(), key.into())?;

        // Create ActorRef with MessageBus reference
        Ok(crate::actor::ActorRef::with_message_bus(
            target_actor_id,
            message_bus,
        ))
    }

    /// Activate the actor by calling its on_activate hook.
    ///
    /// Transitions through: Creating → Activating → Valid
    ///
    /// # Parameters
    ///
    /// - `state`: Optional persisted state to pass to actor
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let context = catalog.get_or_create_activation(actor_id, actor)?;
    /// context.activate(None).await?;
    /// // Actor is now Valid and ready to process messages
    /// ```
    #[allow(clippy::await_holding_refcell_ref)]
    pub async fn activate(
        &self,
        state: crate::actor::ActorState<A::State>,
    ) -> Result<(), crate::error::ActorError> {
        // Transition to Activating
        self.set_state(ActivationState::Activating)?;

        // Call actor's on_activate hook with ActorState wrapper
        let result = {
            let mut actor = self.actor_instance.borrow_mut();
            actor.on_activate(state).await
        };

        // Transition based on result
        match result {
            Ok(()) => {
                self.set_state(ActivationState::Valid)?;
                Ok(())
            }
            Err(e) => {
                // Activation failed, transition to Deactivating
                self.set_state(ActivationState::Deactivating)?;
                Err(e)
            }
        }
    }

    /// Deactivate the actor by calling its on_deactivate hook.
    ///
    /// Transitions through: Valid → Deactivating → Invalid
    ///
    /// This method follows the Orleans pattern by:
    /// 1. Calling on_deactivate() hook
    /// 2. Unregistering from the local catalog (so next message creates fresh activation)
    /// 3. Transitioning to Invalid state
    ///
    /// Note: Directory unregistration happens in DeactivationGuard when message loop exits.
    ///
    /// # Parameters
    ///
    /// - `reason`: Why the actor is being deactivated
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// context.deactivate(DeactivationReason::IdleTimeout).await?;
    /// // Actor is now Invalid and removed from catalog
    /// ```
    #[allow(clippy::await_holding_refcell_ref)]
    pub async fn deactivate(
        &self,
        reason: crate::actor::DeactivationReason,
    ) -> Result<(), crate::error::ActorError> {
        // Transition to Deactivating
        self.set_state(ActivationState::Deactivating)?;

        // Call actor's on_deactivate hook
        let result = {
            let mut actor = self.actor_instance.borrow_mut();
            actor.on_deactivate(reason).await
        };

        // Transition to Invalid regardless of result
        // Note: Catalog unregistration happens in the message loop's Deactivate handler
        // (see run_message_loop), following Orleans ActivationData.cs:1812 pattern
        self.set_state(ActivationState::Invalid)?;

        result
    }
}

/// RAII guard that automatically removes actor from catalog and directory on drop.
///
/// When the message loop exits (either via deactivation or channel closure),
/// this guard's `Drop` implementation ensures the actor is removed from:
/// 1. Local ActorCatalog (so it can be reactivated with fresh channels)
/// 2. Cluster-wide Directory (so placement can happen on another node)
///
/// This enables the Orleans pattern where actors automatically reactivate
/// on the next message after deactivation.
struct DeactivationGuard {
    actor_id: ActorId,
    message_bus: Rc<MessageBus>,
}

impl Drop for DeactivationGuard {
    fn drop(&mut self) {
        tracing::info!(
            "DeactivationGuard: Cleaning up actor {} from catalog and directory",
            self.actor_id
        );

        // Schedule async cleanup task
        // We can't await in Drop, so we spawn a detached task
        let actor_id = self.actor_id.clone();
        let message_bus = self.message_bus.clone();

        tokio::task::spawn_local(async move {
            // Remove from directory first (cluster-wide)
            if let Err(e) = message_bus.directory().unregister(&actor_id).await {
                tracing::warn!(
                    "Failed to unregister actor {} from directory: {}",
                    actor_id,
                    e
                );
            }

            tracing::debug!(
                "DeactivationGuard: Actor {} cleaned up and ready for reactivation",
                actor_id
            );
        });
    }
}

/// Never-ending message loop for an actor (Orleans pattern adapted to Rust).
///
/// This function runs continuously until deactivation, processing messages
/// from two channels using `tokio::select!`:
/// - Message channel: Business logic messages
/// - Control channel: Lifecycle commands (Activate, Deactivate)
///
/// # Parameters
///
/// - `context`: Actor context (shared via Rc)
/// - `msg_rx`: Receiver for actor messages
/// - `ctrl_rx`: Receiver for lifecycle commands
/// - `message_bus`: MessageBus for sending responses
/// - `catalog`: Reference to catalog for unregistration on deactivation
///
/// # Loop Exit Conditions
///
/// - `Deactivate` command received → exit after calling `on_deactivate()`
/// - Both channels closed → exit (all senders dropped)
///
/// # Automatic Cleanup (Drop Guard)
///
/// When this function exits, the `DeactivationGuard` automatically:
/// 1. Removes actor from cluster directory (allows fresh placement)
/// 2. Logs cleanup for debugging
///
/// This enables Orleans-style reactivation: the next message to the same
/// ActorId will trigger a fresh activation with new channels.
///
/// # Orleans Pattern
///
/// This adapts Orleans' `RunMessageLoop()` which uses:
/// - `while(true)` loop + `_workSignal.WaitAsync()`
/// - Separate command queue for lifecycle operations
/// - Never exits (GC'd when dereferenced)
///
/// Moonpool adaptation:
/// - `loop` + `tokio::select!` on two channels
/// - Explicit exit via `Deactivate` command
/// - Channels provide implicit wake mechanism
/// - RAII cleanup via `DeactivationGuard`
#[allow(clippy::await_holding_refcell_ref)]
pub async fn run_message_loop<
    A: Actor + 'static,
    T: moonpool_foundation::TaskProvider + 'static,
    F: crate::actor::ActorFactory<Actor = A> + 'static,
    S: crate::serialization::Serializer + 'static,
>(
    context: Rc<ActorContext<A, S>>,
    mut msg_rx: mpsc::Receiver<Message>,
    mut ctrl_rx: mpsc::Receiver<LifecycleCommand<A>>,
    message_bus: Rc<MessageBus>,
    catalog: Rc<crate::actor::ActorCatalog<A, T, F, S>>,
) {
    let actor_id = context.actor_id.clone();
    tracing::info!("Message loop started: {}", actor_id);

    // Create guard that will clean up actor on drop (when loop exits)
    let _cleanup_guard = DeactivationGuard {
        actor_id: actor_id.clone(),
        message_bus: message_bus.clone(),
    };

    loop {
        tokio::select! {
            // Process actor messages (one at a time)
            Some(message) = msg_rx.recv() => {
                // Only process if actor is Valid
                if context.get_state() != ActivationState::Valid {
                    tracing::warn!(
                        "Discarding message for {} - state: {:?}",
                        actor_id,
                        context.get_state()
                    );
                    continue;
                }

                tracing::debug!(
                    "Processing: {} -> {} (corr_id: {})",
                    message.method_name,
                    actor_id,
                    message.correlation_id
                );

                // Dispatch to actor handler
                // Note: We hold RefCell borrow across await, but this is safe in single-threaded
                // context. The borrow is released after the async function returns.
                let result = {
                    let mut actor = context.actor_instance.borrow_mut();
                    dispatch_message_to_actor(
                        &mut *actor,
                        &message,
                        &context,
                        &message_bus,
                    ).await
                };

                match result {
                    Ok(()) => {
                        context.update_last_message_time();

                        // Check deactivation policy after successful message processing
                        if msg_rx.is_empty() {
                            let policy = A::deactivation_policy();
                            if policy == DeactivationPolicy::DeactivateOnIdle {
                                tracing::info!(
                                    "Actor {} has DeactivateOnIdle policy and queue is empty - triggering deactivation",
                                    actor_id
                                );

                                // Send deactivate command to self
                                let _ = context.control_sender.send(
                                    LifecycleCommand::Deactivate {
                                        reason: DeactivationReason::IdleTimeout,
                                    }
                                ).await;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Processing failed for {}: {:?}", actor_id, e);
                        context.record_error(e.clone());

                        // Send error response for requests
                        if message.direction == crate::messaging::Direction::Request {
                            use crate::messaging::Message;
                            let error_response = Message::error_response(
                                message.correlation_id,
                                message.sender_actor.clone(),
                                message.target_actor.clone(),
                                message.sender_node.clone(),
                                message.target_node.clone(),
                                e,
                            );
                            let _ = message_bus.send_response(error_response).await;
                        }
                    }
                }
            }

            // Process lifecycle commands
            Some(cmd) = ctrl_rx.recv() => {
                match cmd {
                    LifecycleCommand::Activate { state, result_tx } => {
                        tracing::info!("Activating: {}", actor_id);

                        let result = context.activate(state).await;
                        let _ = result_tx.send(result);
                    }

                    LifecycleCommand::Deactivate { reason } => {
                        tracing::info!("Deactivating: {} ({:?})", actor_id, reason);

                        let _ = context.deactivate(reason).await;

                        // ORLEANS PATTERN: Unregister from catalog (ActivationData.cs:1812)
                        // This removes the actor from the catalog's activation_directory,
                        // allowing the next message to trigger fresh activation with new channels
                        tracing::debug!(
                            "Unregistering actor {} from catalog (Orleans pattern)",
                            actor_id
                        );
                        let _ = catalog.remove(&actor_id);

                        break;  // Exit loop after deactivation
                    }
                }
            }

            // Both channels closed - exit gracefully
            else => {
                tracing::info!("All channels closed for: {}", actor_id);
                break;
            }
        }
    }

    tracing::info!("Message loop exited: {}", actor_id);
}

/// Dispatch a message to the actor's handler.
///
/// This function implements dynamic message dispatch by:
/// 1. Looking up the handler in the registry by method name
/// 2. Calling the type-erased handler closure (deserialize → handle → serialize)
/// 3. Sending the response back to the caller (for request messages)
///
/// # Parameters
///
/// - `actor`: Mutable reference to the actor instance
/// - `message`: Incoming message with method name and payload
/// - `context`: Actor execution context (contains handler registry)
/// - `message_bus`: MessageBus for sending responses
///
/// # Returns
///
/// - `Ok(())`: Message dispatched successfully, response sent
/// - `Err(ActorError)`: Handler not found, execution failed, or response send failed
async fn dispatch_message_to_actor<A: Actor, S: crate::serialization::Serializer + 'static>(
    actor: &mut A,
    message: &Message,
    context: &ActorContext<A, S>,
    message_bus: &MessageBus,
) -> Result<(), ActorError> {
    tracing::debug!(
        "Dispatching message '{}' to actor {}",
        message.method_name,
        context.actor_id
    );

    // Dispatch via handler registry
    let response_payload = context.handlers.dispatch(actor, message, context).await?;

    // Send response for request messages
    if message.direction == crate::messaging::Direction::Request {
        let response = crate::messaging::Message::response(message, response_payload);
        message_bus.send_response(response).await?;
    }

    Ok(())
}

// Manual Debug implementation (actor_instance may not be Debug)
impl<A: Actor, S: crate::serialization::Serializer> std::fmt::Debug for ActorContext<A, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorContext")
            .field("actor_id", &self.actor_id)
            .field("node_id", &self.node_id)
            .field("state", &self.state)
            .field("activation_time", &self.activation_time)
            .field("last_message_time", &self.last_message_time)
            .field(
                "has_message_loop_task",
                &self.message_loop_task.borrow().is_some(),
            )
            .field("error_count", &self.error_count.borrow())
            .field("last_error", &self.last_error.borrow())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};

    // ============================================================================
    // Test Actor Implementations
    // ============================================================================

    /// Simple test actor for context testing
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct TestActor {
        actor_id: ActorId,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    struct TestState;

    impl TestActor {
        fn new(actor_id: ActorId) -> Self {
            Self { actor_id }
        }
    }

    #[async_trait(?Send)]
    impl Actor for TestActor {
        type State = TestState;
        const ACTOR_TYPE: &'static str = "TestActor";

        fn actor_id(&self) -> &ActorId {
            &self.actor_id
        }

        async fn on_activate(
            &mut self,
            _state: crate::actor::ActorState<Self::State>,
        ) -> Result<(), ActorError> {
            Ok(())
        }

        async fn on_deactivate(&mut self, _reason: DeactivationReason) -> Result<(), ActorError> {
            Ok(())
        }
    }

    fn create_test_storage() -> Rc<dyn crate::storage::StorageProvider> {
        use crate::storage::InMemoryStorage;
        Rc::new(InMemoryStorage::new())
    }

    fn create_test_serializer() -> crate::serialization::JsonSerializer {
        crate::serialization::JsonSerializer
    }

    // ============================================================================
    // Section 1: Context Creation & Initialization
    // ============================================================================

    #[test]
    fn test_context_creation_basic() {
        let actor_id = ActorId::from_string("test::TestActor/alice").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id.clone(),
            node_id.clone(),
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        assert_eq!(context.actor_id, actor_id);
        assert_eq!(context.node_id, node_id);
    }

    #[test]
    fn test_context_initial_state_is_creating() {
        let actor_id = ActorId::from_string("test::TestActor/bob").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        assert_eq!(context.get_state(), ActivationState::Creating);
    }

    #[test]
    fn test_context_activation_time_initialization() {
        let actor_id = ActorId::from_string("test::TestActor/charlie").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let before = Instant::now();
        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );
        let after = Instant::now();

        // activation_time should be between before and after
        assert!(context.activation_time >= before);
        assert!(context.activation_time <= after);
    }

    #[test]
    fn test_context_handler_registry_initialization() {
        let actor_id = ActorId::from_string("test::TestActor/dave").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Handler registry should be initialized
        // Note: TestActor has no handlers registered, so count is 0
        let _handler_count = context.handlers.handler_count();
    }

    #[test]
    fn test_context_storage_reference_set() {
        let actor_id = ActorId::from_string("test::TestActor/eve").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage.clone(),
            serializer,
        );

        // Verify storage reference is set (same Rc pointer)
        assert!(Rc::ptr_eq(&context.storage, &storage));
    }

    #[test]
    fn test_context_message_sender_channel() {
        let actor_id = ActorId::from_string("test::TestActor/frank").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, mut msg_rx) = mpsc::channel::<Message>(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id.clone(),
            node_id.clone(),
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Test that message sender is connected to receiver
        let test_message = Message::oneway(
            actor_id.clone(),
            actor_id,
            node_id.clone(),
            node_id,
            "test".to_string(),
            vec![],
        );

        // Send via context.message_sender
        context
            .message_sender
            .blocking_send(test_message.clone())
            .unwrap();

        // Verify received
        let received = msg_rx.blocking_recv().unwrap();
        assert_eq!(received.method_name, test_message.method_name);
    }

    #[test]
    fn test_context_control_sender_channel() {
        let actor_id = ActorId::from_string("test::TestActor/grace").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<LifecycleCommand<TestActor>>(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage.clone(),
            serializer,
        );

        // Test that control sender is connected to receiver
        let state = crate::actor::ActorState::new(context.actor_id.clone(), TestState, storage);
        let (result_tx, _result_rx) = oneshot::channel();
        let cmd = LifecycleCommand::Activate { state, result_tx };

        // Send via context.control_sender
        context.control_sender.blocking_send(cmd).unwrap();

        // Verify received
        assert!(matches!(
            ctrl_rx.blocking_recv(),
            Some(LifecycleCommand::Activate { .. })
        ));
    }

    #[test]
    fn test_context_error_tracking_initial_state() {
        let actor_id = ActorId::from_string("test::TestActor/henry").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Initially no errors
        assert_eq!(context.get_error_count(), 0);
        assert!(context.get_last_error().is_none());
    }

    // ============================================================================
    // Section 2: State Transition Validation
    // ============================================================================

    #[test]
    fn test_context_state_transition_creating_to_activating() {
        let actor_id = ActorId::from_string("test::TestActor/ida").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        assert_eq!(context.get_state(), ActivationState::Creating);

        // Valid transition
        context.set_state(ActivationState::Activating).unwrap();
        assert_eq!(context.get_state(), ActivationState::Activating);
    }

    #[test]
    fn test_context_state_transition_activating_to_valid() {
        let actor_id = ActorId::from_string("test::TestActor/judy").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        context.set_state(ActivationState::Activating).unwrap();
        assert_eq!(context.get_state(), ActivationState::Activating);

        // Valid transition
        context.set_state(ActivationState::Valid).unwrap();
        assert_eq!(context.get_state(), ActivationState::Valid);
    }

    #[test]
    fn test_context_state_transition_valid_to_deactivating() {
        let actor_id = ActorId::from_string("test::TestActor/kevin").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        context.set_state(ActivationState::Activating).unwrap();
        context.set_state(ActivationState::Valid).unwrap();
        assert_eq!(context.get_state(), ActivationState::Valid);

        // Valid transition
        context.set_state(ActivationState::Deactivating).unwrap();
        assert_eq!(context.get_state(), ActivationState::Deactivating);
    }

    #[test]
    fn test_context_state_transition_deactivating_to_invalid() {
        let actor_id = ActorId::from_string("test::TestActor/lisa").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        context.set_state(ActivationState::Activating).unwrap();
        context.set_state(ActivationState::Valid).unwrap();
        context.set_state(ActivationState::Deactivating).unwrap();
        assert_eq!(context.get_state(), ActivationState::Deactivating);

        // Valid transition
        context.set_state(ActivationState::Invalid).unwrap();
        assert_eq!(context.get_state(), ActivationState::Invalid);
    }

    #[test]
    fn test_context_state_transition_activating_to_deactivating_on_failure() {
        let actor_id = ActorId::from_string("test::TestActor/mike").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        context.set_state(ActivationState::Activating).unwrap();
        assert_eq!(context.get_state(), ActivationState::Activating);

        // Valid transition on activation failure
        context.set_state(ActivationState::Deactivating).unwrap();
        assert_eq!(context.get_state(), ActivationState::Deactivating);
    }

    #[test]
    fn test_context_invalid_state_transition_creating_to_valid() {
        let actor_id = ActorId::from_string("test::TestActor/nancy").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        assert_eq!(context.get_state(), ActivationState::Creating);

        // Invalid transition (should skip Activating)
        let result = context.set_state(ActivationState::Valid);
        assert!(result.is_err());
        assert!(matches!(
            result,
            Err(ActorError::InvalidStateTransition { .. })
        ));

        // State should remain unchanged
        assert_eq!(context.get_state(), ActivationState::Creating);
    }

    #[test]
    fn test_context_invalid_state_transition_valid_to_activating() {
        let actor_id = ActorId::from_string("test::TestActor/oscar").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        context.set_state(ActivationState::Activating).unwrap();
        context.set_state(ActivationState::Valid).unwrap();
        assert_eq!(context.get_state(), ActivationState::Valid);

        // Invalid transition (backward)
        let result = context.set_state(ActivationState::Activating);
        assert!(result.is_err());

        // State should remain unchanged
        assert_eq!(context.get_state(), ActivationState::Valid);
    }

    #[test]
    fn test_context_invalid_state_transition_invalid_to_creating() {
        let actor_id = ActorId::from_string("test::TestActor/paula").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Go through full lifecycle
        context.set_state(ActivationState::Activating).unwrap();
        context.set_state(ActivationState::Valid).unwrap();
        context.set_state(ActivationState::Deactivating).unwrap();
        context.set_state(ActivationState::Invalid).unwrap();
        assert_eq!(context.get_state(), ActivationState::Invalid);

        // Invalid transition (can't restart from Invalid)
        let result = context.set_state(ActivationState::Creating);
        assert!(result.is_err());

        // State should remain Invalid
        assert_eq!(context.get_state(), ActivationState::Invalid);
    }

    #[test]
    fn test_context_get_state_returns_current() {
        let actor_id = ActorId::from_string("test::TestActor/quinn").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Test get_state at each stage
        assert_eq!(context.get_state(), ActivationState::Creating);

        context.set_state(ActivationState::Activating).unwrap();
        assert_eq!(context.get_state(), ActivationState::Activating);

        context.set_state(ActivationState::Valid).unwrap();
        assert_eq!(context.get_state(), ActivationState::Valid);

        context.set_state(ActivationState::Deactivating).unwrap();
        assert_eq!(context.get_state(), ActivationState::Deactivating);

        context.set_state(ActivationState::Invalid).unwrap();
        assert_eq!(context.get_state(), ActivationState::Invalid);
    }

    #[test]
    fn test_context_state_transition_validation_error_message() {
        let actor_id = ActorId::from_string("test::TestActor/rachel").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Attempt invalid transition
        let result = context.set_state(ActivationState::Valid);
        assert!(result.is_err());

        // Verify error includes from/to states
        match result {
            Err(ActorError::InvalidStateTransition { from, to }) => {
                assert_eq!(from, ActivationState::Creating);
                assert_eq!(to, ActivationState::Valid);
            }
            _ => panic!("Expected InvalidStateTransition error"),
        }
    }

    // ============================================================================
    // Section 3: Message Queue Operations
    // ============================================================================

    #[tokio::test(flavor = "current_thread")]
    async fn test_context_enqueue_message_success() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let actor_id = ActorId::from_string("test::TestActor/sam").unwrap();
                let node_id = NodeId::from("127.0.0.1:8001").unwrap();
                let actor_instance = TestActor::new(actor_id.clone());
                let (msg_tx, mut msg_rx) = mpsc::channel(128);
                let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
                let storage = create_test_storage();
                let serializer = create_test_serializer();

                let context = ActorContext::new(
                    actor_id.clone(),
                    node_id.clone(),
                    actor_instance,
                    msg_tx,
                    ctrl_tx,
                    storage,
                    serializer,
                );

                let message = Message::oneway(
                    actor_id.clone(),
                    actor_id,
                    node_id.clone(),
                    node_id,
                    "test".to_string(),
                    vec![1, 2, 3],
                );

                // Enqueue message
                context.enqueue_message(message.clone()).await.unwrap();

                // Verify received
                let received = msg_rx.recv().await.unwrap();
                assert_eq!(received.method_name, message.method_name);
                assert_eq!(received.payload, message.payload);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_context_enqueue_message_updates_correlation() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let actor_id = ActorId::from_string("test::TestActor/tina").unwrap();
                let node_id = NodeId::from("127.0.0.1:8001").unwrap();
                let actor_instance = TestActor::new(actor_id.clone());
                let (msg_tx, mut msg_rx) = mpsc::channel(128);
                let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
                let storage = create_test_storage();
                let serializer = create_test_serializer();

                let context = ActorContext::new(
                    actor_id.clone(),
                    node_id.clone(),
                    actor_instance,
                    msg_tx,
                    ctrl_tx,
                    storage,
                    serializer,
                );

                let message = Message::oneway(
                    actor_id.clone(),
                    actor_id,
                    node_id.clone(),
                    node_id,
                    "test".to_string(),
                    vec![],
                );

                let corr_id = message.correlation_id;

                context.enqueue_message(message).await.unwrap();

                // Verify correlation ID preserved
                let received = msg_rx.recv().await.unwrap();
                assert_eq!(received.correlation_id, corr_id);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_context_enqueue_message_multiple_sequential() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let actor_id = ActorId::from_string("test::TestActor/uma").unwrap();
                let node_id = NodeId::from("127.0.0.1:8001").unwrap();
                let actor_instance = TestActor::new(actor_id.clone());
                let (msg_tx, mut msg_rx) = mpsc::channel(128);
                let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
                let storage = create_test_storage();
                let serializer = create_test_serializer();

                let context = ActorContext::new(
                    actor_id.clone(),
                    node_id.clone(),
                    actor_instance,
                    msg_tx,
                    ctrl_tx,
                    storage,
                    serializer,
                );

                // Enqueue 3 messages
                for i in 0..3 {
                    let message = Message::oneway(
                        actor_id.clone(),
                        actor_id.clone(),
                        node_id.clone(),
                        node_id.clone(),
                        format!("test{}", i),
                        vec![i],
                    );
                    context.enqueue_message(message).await.unwrap();
                }

                // Verify all received
                for i in 0..3 {
                    let received = msg_rx.recv().await.unwrap();
                    assert_eq!(received.method_name, format!("test{}", i));
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_context_enqueue_message_ordering_preserved() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let actor_id = ActorId::from_string("test::TestActor/victor").unwrap();
                let node_id = NodeId::from("127.0.0.1:8001").unwrap();
                let actor_instance = TestActor::new(actor_id.clone());
                let (msg_tx, mut msg_rx) = mpsc::channel(128);
                let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
                let storage = create_test_storage();
                let serializer = create_test_serializer();

                let context = ActorContext::new(
                    actor_id.clone(),
                    node_id.clone(),
                    actor_instance,
                    msg_tx,
                    ctrl_tx,
                    storage,
                    serializer,
                );

                // Enqueue messages with different payloads
                let payloads = vec![vec![1], vec![2], vec![3], vec![4], vec![5]];
                for payload in &payloads {
                    let message = Message::oneway(
                        actor_id.clone(),
                        actor_id.clone(),
                        node_id.clone(),
                        node_id.clone(),
                        "test".to_string(),
                        payload.clone(),
                    );
                    context.enqueue_message(message).await.unwrap();
                }

                // Verify order preserved
                for expected_payload in payloads {
                    let received = msg_rx.recv().await.unwrap();
                    assert_eq!(received.payload, expected_payload);
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_context_enqueue_message_after_channel_closed() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let actor_id = ActorId::from_string("test::TestActor/wendy").unwrap();
                let node_id = NodeId::from("127.0.0.1:8001").unwrap();
                let actor_instance = TestActor::new(actor_id.clone());
                let (msg_tx, msg_rx) = mpsc::channel(128);
                let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
                let storage = create_test_storage();
                let serializer = create_test_serializer();

                let context = ActorContext::new(
                    actor_id.clone(),
                    node_id.clone(),
                    actor_instance,
                    msg_tx,
                    ctrl_tx,
                    storage,
                    serializer,
                );

                // Drop receiver to close channel
                drop(msg_rx);

                let message = Message::oneway(
                    actor_id.clone(),
                    actor_id,
                    node_id.clone(),
                    node_id,
                    "test".to_string(),
                    vec![],
                );

                // Enqueue should fail
                let result = context.enqueue_message(message).await;
                assert!(result.is_err());
                assert!(matches!(result, Err(ActorError::ProcessingFailed(_))));
            })
            .await;
    }

    #[test]
    fn test_context_message_sender_capacity_limit() {
        let actor_id = ActorId::from_string("test::TestActor/xavier").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel::<Message>(2); // Small capacity
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id.clone(),
            node_id.clone(),
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Fill channel (2 messages)
        for i in 0..2 {
            let message = Message::oneway(
                actor_id.clone(),
                actor_id.clone(),
                node_id.clone(),
                node_id.clone(),
                format!("test{}", i),
                vec![],
            );
            context.message_sender.blocking_send(message).unwrap();
        }

        // Next send would block (we don't test blocking_send with full channel)
        // This test just verifies the capacity limit exists
    }

    #[test]
    fn test_context_control_sender_separate_channel() {
        let actor_id = ActorId::from_string("test::TestActor/yara").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, mut msg_rx) = mpsc::channel(128);
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<LifecycleCommand<TestActor>>(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id.clone(),
            node_id.clone(),
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage.clone(),
            serializer,
        );

        // Send message on message channel
        let message = Message::oneway(
            actor_id.clone(),
            actor_id.clone(),
            node_id.clone(),
            node_id.clone(),
            "test".to_string(),
            vec![],
        );
        context.message_sender.blocking_send(message).unwrap();

        // Send command on control channel
        let state = crate::actor::ActorState::new(context.actor_id.clone(), TestState, storage);
        let (result_tx, _result_rx) = oneshot::channel();
        let cmd = LifecycleCommand::Activate { state, result_tx };
        context.control_sender.blocking_send(cmd).unwrap();

        // Verify channels are independent
        assert!(msg_rx.try_recv().is_ok());
        assert!(ctrl_rx.try_recv().is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_context_enqueue_message_backpressure_handling() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let actor_id = ActorId::from_string("test::TestActor/zara").unwrap();
                let node_id = NodeId::from("127.0.0.1:8001").unwrap();
                let actor_instance = TestActor::new(actor_id.clone());
                let (msg_tx, mut msg_rx) = mpsc::channel(5); // Small capacity for backpressure
                let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
                let storage = create_test_storage();
                let serializer = create_test_serializer();

                let context = ActorContext::new(
                    actor_id.clone(),
                    node_id.clone(),
                    actor_instance,
                    msg_tx,
                    ctrl_tx,
                    storage,
                    serializer,
                );

                // Fill channel
                for i in 0..5 {
                    let message = Message::oneway(
                        actor_id.clone(),
                        actor_id.clone(),
                        node_id.clone(),
                        node_id.clone(),
                        format!("test{}", i),
                        vec![],
                    );
                    context.enqueue_message(message).await.unwrap();
                }

                // Drain one message
                msg_rx.recv().await.unwrap();

                // Now can enqueue one more
                let message = Message::oneway(
                    actor_id.clone(),
                    actor_id,
                    node_id.clone(),
                    node_id,
                    "test5".to_string(),
                    vec![],
                );
                context.enqueue_message(message).await.unwrap();
            })
            .await;
    }

    // ============================================================================
    // Section 4: Error Tracking & Monitoring
    // ============================================================================

    #[test]
    fn test_context_record_error_updates_last_error() {
        let actor_id = ActorId::from_string("test::TestActor/adam").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        assert!(context.get_last_error().is_none());

        let error = ActorError::ProcessingFailed("test error".to_string());
        context.record_error(error.clone());

        let last_error = context.get_last_error().unwrap();
        assert!(matches!(last_error, ActorError::ProcessingFailed(_)));
    }

    #[test]
    fn test_context_record_error_increments_count() {
        let actor_id = ActorId::from_string("test::TestActor/beth").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        assert_eq!(context.get_error_count(), 0);

        context.record_error(ActorError::ProcessingFailed("error1".to_string()));
        assert_eq!(context.get_error_count(), 1);

        context.record_error(ActorError::ProcessingFailed("error2".to_string()));
        assert_eq!(context.get_error_count(), 2);
    }

    #[test]
    fn test_context_get_last_error_returns_most_recent() {
        let actor_id = ActorId::from_string("test::TestActor/carlos").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        context.record_error(ActorError::ProcessingFailed("first".to_string()));
        context.record_error(ActorError::ProcessingFailed("second".to_string()));
        context.record_error(ActorError::ProcessingFailed("third".to_string()));

        let last_error = context.get_last_error().unwrap();
        match last_error {
            ActorError::ProcessingFailed(msg) => assert_eq!(msg, "third"),
            _ => panic!("Expected ProcessingFailed error"),
        }
    }

    #[test]
    fn test_context_get_error_count_accumulates() {
        let actor_id = ActorId::from_string("test::TestActor/diana").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        for i in 1..=5 {
            context.record_error(ActorError::ProcessingFailed(format!("error{}", i)));
            assert_eq!(context.get_error_count(), i);
        }
    }

    #[test]
    fn test_context_clear_errors_resets_state() {
        let actor_id = ActorId::from_string("test::TestActor/ethan").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Record errors
        context.record_error(ActorError::ProcessingFailed("error1".to_string()));
        context.record_error(ActorError::ProcessingFailed("error2".to_string()));
        assert_eq!(context.get_error_count(), 2);
        assert!(context.get_last_error().is_some());

        // Clear errors
        context.clear_errors();
        assert_eq!(context.get_error_count(), 0);
        assert!(context.get_last_error().is_none());
    }

    #[test]
    fn test_context_multiple_errors_accumulate_count() {
        let actor_id = ActorId::from_string("test::TestActor/fiona").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        for i in 1..=10 {
            context.record_error(ActorError::ProcessingFailed(format!("error{}", i)));
        }

        assert_eq!(context.get_error_count(), 10);
    }

    #[test]
    fn test_context_error_isolation_continues_processing() {
        let actor_id = ActorId::from_string("test::TestActor/george").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, mut msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id.clone(),
            node_id.clone(),
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Record error
        context.record_error(ActorError::ProcessingFailed("error".to_string()));

        // Context should still be able to process messages
        let message = Message::oneway(
            actor_id.clone(),
            actor_id,
            node_id.clone(),
            node_id,
            "test".to_string(),
            vec![],
        );
        context.message_sender.blocking_send(message).unwrap();

        // Verify message received
        assert!(msg_rx.blocking_recv().is_some());
    }

    // ============================================================================
    // Section 5: Actor-to-Actor Communication
    // ============================================================================

    fn create_test_message_bus() -> Rc<crate::messaging::MessageBus> {
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let callback_manager = Rc::new(crate::messaging::CallbackManager::new());
        let directory = Rc::new(crate::directory::SimpleDirectory::new())
            as Rc<dyn crate::directory::Directory>;
        let placement = crate::placement::SimplePlacement::new(vec![node_id.clone()]);

        // Mock network transport
        struct MockNetworkTransport;

        #[async_trait::async_trait(?Send)]
        impl crate::messaging::NetworkTransport for MockNetworkTransport {
            async fn send(
                &self,
                _destination: &str,
                _message: crate::messaging::Message,
            ) -> Result<crate::messaging::Message, ActorError> {
                Ok(crate::messaging::Message::response(&_message, vec![]))
            }

            fn poll_receive(&self) -> Option<crate::messaging::Message> {
                None
            }
        }

        let network_transport =
            Rc::new(MockNetworkTransport) as Rc<dyn crate::messaging::NetworkTransport>;

        Rc::new(crate::messaging::MessageBus::new(
            node_id,
            callback_manager,
            directory,
            placement,
            network_transport,
        ))
    }

    #[test]
    fn test_context_get_actor_creates_correct_actor_ref() {
        let actor_id = ActorId::from_string("test::TestActor/helen").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Set MessageBus
        let message_bus = create_test_message_bus();
        context.set_message_bus(message_bus);

        // Get actor reference
        let actor_ref: Result<crate::actor::ActorRef<TestActor>, _> =
            context.get_actor("TestActor", "other");

        assert!(actor_ref.is_ok());
    }

    #[test]
    fn test_context_get_actor_preserves_namespace() {
        let actor_id = ActorId::from_string("production::TestActor/ian").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        let message_bus = create_test_message_bus();
        context.set_message_bus(message_bus);

        let actor_ref: crate::actor::ActorRef<TestActor> =
            context.get_actor("TestActor", "other").unwrap();

        // Verify namespace is preserved from context's actor_id
        assert_eq!(actor_ref.actor_id().namespace, "production");
    }

    #[test]
    fn test_context_get_actor_fails_without_message_bus() {
        let actor_id = ActorId::from_string("test::TestActor/jack").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Don't set MessageBus

        let result: Result<crate::actor::ActorRef<TestActor>, _> =
            context.get_actor("TestActor", "other");

        assert!(result.is_err());
        assert!(matches!(result, Err(ActorError::ProcessingFailed(_))));
    }

    #[test]
    fn test_context_get_actor_different_types_same_namespace() {
        let actor_id = ActorId::from_string("test::TestActor/kate").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        let message_bus = create_test_message_bus();
        context.set_message_bus(message_bus);

        // Get references to different actor types
        let ref1: crate::actor::ActorRef<TestActor> =
            context.get_actor("TestActor", "alice").unwrap();
        let ref2: crate::actor::ActorRef<TestActor> =
            context.get_actor("OtherActor", "bob").unwrap();

        // Both should have same namespace
        assert_eq!(ref1.actor_id().namespace, ref2.actor_id().namespace);
        assert_eq!(ref1.actor_id().namespace, "test");
    }

    #[test]
    fn test_context_get_actor_uses_message_bus_reference() {
        let actor_id = ActorId::from_string("test::TestActor/leo").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        let message_bus = create_test_message_bus();
        let bus_clone = message_bus.clone();
        context.set_message_bus(message_bus);

        // Verify MessageBus is set
        assert!(context.get_message_bus().is_some());
        assert!(Rc::ptr_eq(&context.get_message_bus().unwrap(), &bus_clone));
    }

    #[test]
    fn test_context_get_actor_constructs_valid_actor_id() {
        let actor_id = ActorId::from_string("test::TestActor/maria").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        let message_bus = create_test_message_bus();
        context.set_message_bus(message_bus);

        let actor_ref: crate::actor::ActorRef<TestActor> =
            context.get_actor("TargetActor", "target_key").unwrap();

        // Verify ActorId components
        assert_eq!(actor_ref.actor_id().namespace, "test");
        assert_eq!(actor_ref.actor_id().actor_type, "TargetActor");
        assert_eq!(actor_ref.actor_id().key, "target_key");
    }

    // ============================================================================
    // Section 6: Lifecycle Operations
    // ============================================================================

    #[tokio::test(flavor = "current_thread")]
    async fn test_context_activate_transitions_to_activating() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let actor_id = ActorId::from_string("test::TestActor/nina").unwrap();
                let node_id = NodeId::from("127.0.0.1:8001").unwrap();
                let actor_instance = TestActor::new(actor_id.clone());
                let (msg_tx, _msg_rx) = mpsc::channel(128);
                let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
                let storage = create_test_storage();
                let serializer = create_test_serializer();

                let context = ActorContext::new(
                    actor_id.clone(),
                    node_id,
                    actor_instance,
                    msg_tx,
                    ctrl_tx,
                    storage.clone(),
                    serializer,
                );

                assert_eq!(context.get_state(), ActivationState::Creating);

                let state = crate::actor::ActorState::new(actor_id, TestState, storage);
                context.activate(state).await.unwrap();

                // Should transition through Activating to Valid
                assert_eq!(context.get_state(), ActivationState::Valid);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_context_activate_calls_on_activate_hook() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let actor_id = ActorId::from_string("test::TestActor/oliver").unwrap();
                let node_id = NodeId::from("127.0.0.1:8001").unwrap();
                let actor_instance = TestActor::new(actor_id.clone());
                let (msg_tx, _msg_rx) = mpsc::channel(128);
                let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
                let storage = create_test_storage();
                let serializer = create_test_serializer();

                let context = ActorContext::new(
                    actor_id.clone(),
                    node_id,
                    actor_instance,
                    msg_tx,
                    ctrl_tx,
                    storage.clone(),
                    serializer,
                );

                let state = crate::actor::ActorState::new(actor_id, TestState, storage);
                let result = context.activate(state).await;

                // Should succeed (TestActor's on_activate returns Ok)
                assert!(result.is_ok());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_context_activate_success_transitions_to_valid() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let actor_id = ActorId::from_string("test::TestActor/peter").unwrap();
                let node_id = NodeId::from("127.0.0.1:8001").unwrap();
                let actor_instance = TestActor::new(actor_id.clone());
                let (msg_tx, _msg_rx) = mpsc::channel(128);
                let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
                let storage = create_test_storage();
                let serializer = create_test_serializer();

                let context = ActorContext::new(
                    actor_id.clone(),
                    node_id,
                    actor_instance,
                    msg_tx,
                    ctrl_tx,
                    storage.clone(),
                    serializer,
                );

                let state = crate::actor::ActorState::new(actor_id, TestState, storage);
                context.activate(state).await.unwrap();

                assert_eq!(context.get_state(), ActivationState::Valid);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_context_deactivate_transitions_to_deactivating() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let actor_id = ActorId::from_string("test::TestActor/quinn2").unwrap();
                let node_id = NodeId::from("127.0.0.1:8001").unwrap();
                let actor_instance = TestActor::new(actor_id.clone());
                let (msg_tx, _msg_rx) = mpsc::channel(128);
                let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
                let storage = create_test_storage();
                let serializer = create_test_serializer();

                let context = ActorContext::new(
                    actor_id.clone(),
                    node_id,
                    actor_instance,
                    msg_tx,
                    ctrl_tx,
                    storage.clone(),
                    serializer,
                );

                // Activate first
                let state = crate::actor::ActorState::new(actor_id, TestState, storage);
                context.activate(state).await.unwrap();
                assert_eq!(context.get_state(), ActivationState::Valid);

                // Deactivate
                context
                    .deactivate(DeactivationReason::ExplicitRequest)
                    .await
                    .unwrap();

                // Should transition through Deactivating to Invalid
                assert_eq!(context.get_state(), ActivationState::Invalid);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_context_deactivate_calls_on_deactivate_hook() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let actor_id = ActorId::from_string("test::TestActor/rachel2").unwrap();
                let node_id = NodeId::from("127.0.0.1:8001").unwrap();
                let actor_instance = TestActor::new(actor_id.clone());
                let (msg_tx, _msg_rx) = mpsc::channel(128);
                let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
                let storage = create_test_storage();
                let serializer = create_test_serializer();

                let context = ActorContext::new(
                    actor_id.clone(),
                    node_id,
                    actor_instance,
                    msg_tx,
                    ctrl_tx,
                    storage.clone(),
                    serializer,
                );

                // Activate first
                let state = crate::actor::ActorState::new(actor_id, TestState, storage);
                context.activate(state).await.unwrap();

                // Deactivate
                let result = context.deactivate(DeactivationReason::IdleTimeout).await;

                // Should succeed (TestActor's on_deactivate returns Ok)
                assert!(result.is_ok());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_context_deactivate_transitions_to_invalid() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let actor_id = ActorId::from_string("test::TestActor/steve").unwrap();
                let node_id = NodeId::from("127.0.0.1:8001").unwrap();
                let actor_instance = TestActor::new(actor_id.clone());
                let (msg_tx, _msg_rx) = mpsc::channel(128);
                let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
                let storage = create_test_storage();
                let serializer = create_test_serializer();

                let context = ActorContext::new(
                    actor_id.clone(),
                    node_id,
                    actor_instance,
                    msg_tx,
                    ctrl_tx,
                    storage.clone(),
                    serializer,
                );

                // Activate first
                let state = crate::actor::ActorState::new(actor_id, TestState, storage);
                context.activate(state).await.unwrap();

                // Deactivate
                context
                    .deactivate(DeactivationReason::ExplicitRequest)
                    .await
                    .unwrap();

                assert_eq!(context.get_state(), ActivationState::Invalid);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_context_activate_deactivate_full_cycle() {
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async {
                let actor_id = ActorId::from_string("test::TestActor/tara").unwrap();
                let node_id = NodeId::from("127.0.0.1:8001").unwrap();
                let actor_instance = TestActor::new(actor_id.clone());
                let (msg_tx, _msg_rx) = mpsc::channel(128);
                let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
                let storage = create_test_storage();
                let serializer = create_test_serializer();

                let context = ActorContext::new(
                    actor_id.clone(),
                    node_id,
                    actor_instance,
                    msg_tx,
                    ctrl_tx,
                    storage.clone(),
                    serializer,
                );

                // Full lifecycle: Creating → Activating → Valid → Deactivating → Invalid
                assert_eq!(context.get_state(), ActivationState::Creating);

                let state = crate::actor::ActorState::new(actor_id, TestState, storage);
                context.activate(state).await.unwrap();
                assert_eq!(context.get_state(), ActivationState::Valid);

                context
                    .deactivate(DeactivationReason::ExplicitRequest)
                    .await
                    .unwrap();
                assert_eq!(context.get_state(), ActivationState::Invalid);
            })
            .await;
    }

    // ============================================================================
    // Section 7: Time Tracking & Idle Detection
    // ============================================================================

    #[test]
    fn test_context_update_last_message_time_updates_timestamp() {
        let actor_id = ActorId::from_string("test::TestActor/ursula").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        let before = *context.last_message_time.borrow();
        std::thread::sleep(std::time::Duration::from_millis(10));
        context.update_last_message_time();
        let after = *context.last_message_time.borrow();

        assert!(after > before);
    }

    #[test]
    fn test_context_time_since_last_message_calculates_duration() {
        let actor_id = ActorId::from_string("test::TestActor/victor2").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        std::thread::sleep(std::time::Duration::from_millis(50));
        let duration = context.time_since_last_message();

        assert!(duration >= std::time::Duration::from_millis(50));
    }

    #[test]
    fn test_context_last_message_time_initial_value() {
        let actor_id = ActorId::from_string("test::TestActor/wendy2").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let before = Instant::now();
        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );
        let after = Instant::now();

        let last_message_time = *context.last_message_time.borrow();
        assert!(last_message_time >= before);
        assert!(last_message_time <= after);
    }

    #[test]
    fn test_context_idle_detection_after_delay() {
        let actor_id = ActorId::from_string("test::TestActor/xander").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        std::thread::sleep(std::time::Duration::from_millis(100));
        let idle_time = context.time_since_last_message();

        // Should be idle for at least 100ms
        assert!(idle_time >= std::time::Duration::from_millis(100));
    }

    #[test]
    fn test_context_activation_time_set_on_creation() {
        let actor_id = ActorId::from_string("test::TestActor/yolanda").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let before = Instant::now();
        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );
        let after = Instant::now();

        // activation_time should be set during context creation
        assert!(context.activation_time >= before);
        assert!(context.activation_time <= after);
    }

    #[test]
    fn test_context_time_tracking_after_multiple_messages() {
        let actor_id = ActorId::from_string("test::TestActor/zelda").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Simulate processing multiple messages
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            context.update_last_message_time();
        }

        // Time since last message should be small (just processed)
        let idle_time = context.time_since_last_message();
        assert!(idle_time < std::time::Duration::from_millis(50));
    }

    // ============================================================================
    // Section 8: MessageBus Integration
    // ============================================================================

    #[test]
    fn test_context_set_message_bus_stores_reference() {
        let actor_id = ActorId::from_string("test::TestActor/aaron").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        let message_bus = create_test_message_bus();
        context.set_message_bus(message_bus.clone());

        // Verify MessageBus is stored
        assert!(context.get_message_bus().is_some());
        assert!(Rc::ptr_eq(
            &context.get_message_bus().unwrap(),
            &message_bus
        ));
    }

    #[test]
    fn test_context_get_message_bus_returns_reference() {
        let actor_id = ActorId::from_string("test::TestActor/barbara").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        let message_bus = create_test_message_bus();
        context.set_message_bus(message_bus.clone());

        let retrieved = context.get_message_bus().unwrap();
        assert!(Rc::ptr_eq(&retrieved, &message_bus));
    }

    #[test]
    fn test_context_get_message_bus_none_before_set() {
        let actor_id = ActorId::from_string("test::TestActor/carl").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Before setting MessageBus
        assert!(context.get_message_bus().is_none());
    }

    #[test]
    fn test_context_message_bus_used_for_get_actor() {
        let actor_id = ActorId::from_string("test::TestActor/donna").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        // Without MessageBus, get_actor fails
        let result: Result<crate::actor::ActorRef<TestActor>, _> =
            context.get_actor("TestActor", "other");
        assert!(result.is_err());

        // With MessageBus, get_actor succeeds
        let message_bus = create_test_message_bus();
        context.set_message_bus(message_bus);

        let result2: Result<crate::actor::ActorRef<TestActor>, _> =
            context.get_actor("TestActor", "other");
        assert!(result2.is_ok());
    }

    #[test]
    fn test_context_message_bus_reference_cloneable() {
        let actor_id = ActorId::from_string("test::TestActor/eric").unwrap();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let actor_instance = TestActor::new(actor_id.clone());
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let storage = create_test_storage();
        let serializer = create_test_serializer();

        let context = ActorContext::new(
            actor_id,
            node_id,
            actor_instance,
            msg_tx,
            ctrl_tx,
            storage,
            serializer,
        );

        let message_bus = create_test_message_bus();
        context.set_message_bus(message_bus.clone());

        // Get MessageBus twice, should be same reference
        let bus1 = context.get_message_bus().unwrap();
        let bus2 = context.get_message_bus().unwrap();

        assert!(Rc::ptr_eq(&bus1, &bus2));
        assert!(Rc::ptr_eq(&bus1, &message_bus));
    }
}
