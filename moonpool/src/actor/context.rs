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
pub struct ActorContext<A: Actor> {
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
    pub handlers: crate::actor::HandlerRegistry<A>,

    /// Storage provider for actor state persistence.
    ///
    /// Actors can use this to persist state changes via `ActorState<T>` wrapper.
    /// Shared across all actors in the same runtime.
    pub storage: Rc<dyn crate::storage::StorageProvider>,
}

impl<A: Actor> ActorContext<A> {
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
    pub fn new(
        actor_id: ActorId,
        node_id: NodeId,
        actor_instance: A,
        message_sender: mpsc::Sender<Message>,
        control_sender: mpsc::Sender<LifecycleCommand<A>>,
        storage: Rc<dyn crate::storage::StorageProvider>,
    ) -> Self {
        let now = Instant::now();

        // Initialize handler registry by calling the actor's register_handlers() method
        let mut handlers = crate::actor::HandlerRegistry::new();
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
>(
    context: Rc<ActorContext<A>>,
    mut msg_rx: mpsc::Receiver<Message>,
    mut ctrl_rx: mpsc::Receiver<LifecycleCommand<A>>,
    message_bus: Rc<MessageBus>,
    catalog: Rc<crate::actor::ActorCatalog<A, T, F>>,
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
async fn dispatch_message_to_actor<A: Actor>(
    actor: &mut A,
    message: &Message,
    context: &ActorContext<A>,
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
impl<A: Actor> std::fmt::Debug for ActorContext<A> {
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
