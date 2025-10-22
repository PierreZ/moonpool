//! Actor context and execution environment.
//!
//! This module provides the `ActorContext` which holds per-actor state and metadata.

use crate::actor::{ActivationState, Actor, ActorId, NodeId};
use crate::error::ActorError;
use crate::messaging::{Message, MessageBus};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Instant;

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
/// - `message_queue`: FIFO queue for incoming messages
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
/// - Queue never drops messages (unbounded, monitored)
/// - `last_message_time` updated on every message processing
/// - Actor only removed from catalog when state == Invalid
pub struct ActorContext<A: Actor> {
    /// Actor's unique identifier.
    pub actor_id: ActorId,

    /// Hosting node identifier.
    pub node_id: NodeId,

    /// Current lifecycle state (interior mutability via RefCell).
    pub state: RefCell<ActivationState>,

    /// Unbounded FIFO queue for incoming messages.
    ///
    /// Messages are enqueued when actor is in any state, but only
    /// dequeued and processed when state == Valid.
    pub message_queue: RefCell<VecDeque<Message>>,

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

    /// Flag indicating whether message processing loop is running.
    ///
    /// Prevents spawning multiple processing tasks for the same actor.
    pub is_processing: RefCell<bool>,

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
    /// - `message_queue`: Empty
    /// - `activation_time`: Current time
    /// - `last_message_time`: Current time
    /// - `is_processing`: `false`
    pub fn new(actor_id: ActorId, node_id: NodeId, actor_instance: A) -> Self {
        let now = Instant::now();
        Self {
            actor_id,
            node_id,
            state: RefCell::new(ActivationState::Creating),
            message_queue: RefCell::new(VecDeque::new()),
            actor_instance: RefCell::new(actor_instance),
            activation_time: now,
            last_message_time: RefCell::new(now),
            is_processing: RefCell::new(false),
            message_bus: RefCell::new(None),
            last_error: RefCell::new(None),
            error_count: RefCell::new(0),
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

    /// Enqueue a message for processing.
    ///
    /// Messages are added to the queue regardless of actor state.
    /// The message processing loop will handle them when actor is Valid.
    ///
    /// # Parameters
    ///
    /// - `message`: The message to enqueue
    ///
    /// # Note
    ///
    /// This does NOT automatically spawn a processing task. The caller
    /// (typically ActorCatalog) must call `spawn_message_processor()` if needed.
    pub fn enqueue_message(&self, message: Message) {
        self.message_queue.borrow_mut().push_back(message);
    }

    /// Dequeue the next message for processing.
    ///
    /// # Returns
    ///
    /// - `Some(message)`: Next message in queue
    /// - `None`: Queue is empty
    pub fn dequeue_message(&self) -> Option<Message> {
        self.message_queue.borrow_mut().pop_front()
    }

    /// Get the number of queued messages.
    pub fn queue_length(&self) -> usize {
        self.message_queue.borrow().len()
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

    /// Check if message processing loop is running.
    pub fn is_processing(&self) -> bool {
        *self.is_processing.borrow()
    }

    /// Set message processing flag.
    pub fn set_processing(&self, value: bool) {
        *self.is_processing.borrow_mut() = value;
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
    pub async fn activate(&self, state: Option<A::State>) -> Result<(), crate::error::ActorError> {
        // Transition to Activating
        self.set_state(ActivationState::Activating)?;

        // Call actor's on_activate hook
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
    /// # Parameters
    ///
    /// - `reason`: Why the actor is being deactivated
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// context.deactivate(DeactivationReason::IdleTimeout).await?;
    /// catalog.remove(&actor_id)?;
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
        self.set_state(ActivationState::Invalid)?;

        result
    }

    /// Process messages from the queue until empty.
    ///
    /// This method dequeues and processes messages sequentially using the
    /// provided dispatch function. It's designed to be called by ActorCatalog
    /// when messages are enqueued.
    ///
    /// # Type Parameters
    ///
    /// - `F`: Async dispatch function that processes a single message
    ///
    /// # Parameters
    ///
    /// - `dispatch_fn`: Function that dispatches message to actor's handlers
    ///
    /// # Returns
    ///
    /// - `Ok(count)`: Number of messages processed
    /// - `Err(ActorError)`: Processing failed
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // In ActorCatalog, provide dispatch closure
    /// context.process_message_queue(|ctx, msg, bus| async move {
    ///     ctx.dispatch_message_impl(msg, bus).await
    /// }).await?;
    /// ```
    pub async fn process_message_queue<F, Fut>(
        self: &Rc<Self>,
        dispatch_fn: F,
    ) -> Result<usize, ActorError>
    where
        F: Fn(Rc<Self>, Message, Rc<MessageBus>) -> Fut,
        Fut: std::future::Future<Output = Result<(), ActorError>>,
    {
        let mut count = 0;

        loop {
            // Check if we should continue processing
            if self.get_state() != ActivationState::Valid {
                tracing::warn!(
                    "Actor {} not in Valid state, stopping message processing",
                    self.actor_id
                );
                break;
            }

            // Dequeue next message
            let message = match self.dequeue_message() {
                Some(msg) => msg,
                None => break, // Queue empty
            };

            // Get message bus
            let message_bus = match self.get_message_bus() {
                Some(bus) => bus,
                None => {
                    tracing::error!(
                        "No MessageBus set for actor {}, cannot process messages",
                        self.actor_id
                    );
                    return Err(ActorError::ProcessingFailed(
                        "MessageBus not configured".to_string(),
                    ));
                }
            };

            // Dispatch message using provided function
            tracing::debug!(
                "Processing message for actor {}: {} (correlation: {})",
                self.actor_id,
                message.method_name,
                message.correlation_id
            );

            if let Err(e) = dispatch_fn(self.clone(), message, message_bus).await {
                tracing::error!(
                    "Message processing failed for actor {}: {:?}",
                    self.actor_id,
                    e
                );
                // Record error for monitoring
                self.record_error(e);
                // Continue processing remaining messages even if one fails
            }

            count += 1;
        }

        tracing::debug!("Processed {} messages for actor {}", count, self.actor_id);
        Ok(count)
    }
}

// Manual Debug implementation (actor_instance may not be Debug)
impl<A: Actor> std::fmt::Debug for ActorContext<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorContext")
            .field("actor_id", &self.actor_id)
            .field("node_id", &self.node_id)
            .field("state", &self.state)
            .field("queue_length", &self.queue_length())
            .field("activation_time", &self.activation_time)
            .field("last_message_time", &self.last_message_time)
            .field("is_processing", &self.is_processing.borrow())
            .field("error_count", &self.error_count.borrow())
            .field("last_error", &self.last_error.borrow())
            .finish()
    }
}
