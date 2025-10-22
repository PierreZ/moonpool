//! Actor context and execution environment.
//!
//! This module provides the `ActorContext` which holds per-actor state and metadata.

use crate::actor::{ActivationState, Actor, ActorId, NodeId};
use crate::messaging::Message;
use std::cell::RefCell;
use std::collections::VecDeque;
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
            .finish()
    }
}
