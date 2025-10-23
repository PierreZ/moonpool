//! Actor catalog with double-check locking for activation management.
//!
//! This module provides the `ActorCatalog` which manages local actor activations
//! using the double-check locking pattern from Orleans to prevent duplicate instances.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────┐
//! │ ActorCatalog                        │
//! │                                     │
//! │  ┌───────────────────────────────┐  │
//! │  │ ActivationDirectory           │  │
//! │  │  (ActorId → ActorContext<A>)  │  │
//! │  └───────────────────────────────┘  │
//! │                                     │
//! │  ┌───────────────────────────────┐  │
//! │  │ activation_lock: RefCell<()>  │  │
//! │  │  (Coarse lock for creation)   │  │
//! │  └───────────────────────────────┘  │
//! └─────────────────────────────────────┘
//! ```
//!
//! # Double-Check Locking Pattern
//!
//! ```text
//! Fast Path (99% case):
//!   1. Check activation_directory (no lock)
//!   2. If found → return activation
//!
//! Slow Path (1% case - first access):
//!   1. Acquire activation_lock
//!   2. Double-check activation_directory
//!   3. If found → return activation (race handled)
//!   4. Create new ActorContext
//!   5. Record in activation_directory
//!   6. Release lock
//!   7. Return activation for activation outside lock
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use moonpool::prelude::*;
//! use moonpool::actor::ActorCatalog;
//!
//! // Create catalog
//! let node_id = NodeId::from("127.0.0.1:8001")?;
//! let catalog = ActorCatalog::<BankAccountActor>::new(node_id);
//!
//! // Get or create activation (double-check locking)
//! let actor_id = ActorId::from_string("prod::BankAccount/alice")?;
//! let context = catalog.get_or_create_activation(actor_id)?;
//!
//! // Activate outside lock
//! context.activate().await?;
//! ```

use crate::actor::{Actor, ActorContext, ActorId, NodeId};
use crate::error::ActorError;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Local actor registry mapping ActorId to ActorContext.
///
/// `ActivationDirectory` is a simple hash map that tracks all locally-activated
/// actors on this node. It provides O(1) lookup by ActorId.
///
/// # Thread Safety
///
/// Uses `RefCell` for single-threaded interior mutability (no Send/Sync).
///
/// # Example
///
/// ```rust,ignore
/// let directory = ActivationDirectory::new();
///
/// // Record new activation
/// let context = Rc::new(ActorContext::new(actor_id, node_id));
/// directory.record_new_target(context.clone());
///
/// // Find activation
/// if let Some(found) = directory.find_target(&actor_id) {
///     println!("Actor is active: {}", found.actor_id);
/// }
///
/// // Remove activation
/// directory.remove_target(&actor_id);
/// ```
pub struct ActivationDirectory<A: Actor> {
    /// Map from ActorId to ActorContext.
    ///
    /// Uses string key for efficient hashing (ActorId formatted as "namespace::actor_type/key").
    activations: RefCell<HashMap<String, Rc<ActorContext<A>>>>,

    /// Count of active activations.
    ///
    /// Tracked separately for efficient metrics without iterating the map.
    count: RefCell<usize>,
}

impl<A: Actor> ActivationDirectory<A> {
    /// Create a new empty activation directory.
    pub fn new() -> Self {
        Self {
            activations: RefCell::new(HashMap::new()),
            count: RefCell::new(0),
        }
    }

    /// Get the number of active activations.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let directory = ActivationDirectory::new();
    /// assert_eq!(directory.count(), 0);
    ///
    /// directory.record_new_target(context);
    /// assert_eq!(directory.count(), 1);
    /// ```
    pub fn count(&self) -> usize {
        *self.count.borrow()
    }

    /// Find an activation by ActorId.
    ///
    /// Returns `Some(context)` if the actor is active locally, `None` otherwise.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The ActorId to look up
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if let Some(context) = directory.find_target(&actor_id) {
    ///     // Actor is active, process message
    ///     context.enqueue_message(message);
    /// } else {
    ///     // Actor not active, create activation
    ///     let context = catalog.get_or_create_activation(actor_id)?;
    /// }
    /// ```
    pub fn find_target(&self, actor_id: &ActorId) -> Option<Rc<ActorContext<A>>> {
        let key = storage_key(actor_id);
        self.activations.borrow().get(&key).cloned()
    }

    /// Record a new activation.
    ///
    /// Inserts the activation into the directory and increments the count.
    /// If an activation with the same ActorId already exists, it is replaced
    /// (this should not happen in normal operation due to double-check locking).
    ///
    /// # Parameters
    ///
    /// - `target`: The ActorContext to record
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let context = Rc::new(ActorContext::new(actor_id, node_id));
    /// directory.record_new_target(context);
    /// ```
    pub fn record_new_target(&self, target: Rc<ActorContext<A>>) {
        let key = storage_key(&target.actor_id);
        let mut activations = self.activations.borrow_mut();

        // Only increment count if this is a new activation
        if activations.insert(key, target).is_none() {
            *self.count.borrow_mut() += 1;
        }
    }

    /// Remove an activation.
    ///
    /// Returns `true` if the activation was found and removed, `false` otherwise.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The ActorId to remove
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Deactivate actor
    /// if directory.remove_target(&actor_id) {
    ///     println!("Actor deactivated: {}", actor_id);
    /// }
    /// ```
    pub fn remove_target(&self, actor_id: &ActorId) -> bool {
        let key = storage_key(actor_id);
        let mut activations = self.activations.borrow_mut();

        if activations.remove(&key).is_some() {
            *self.count.borrow_mut() -= 1;
            true
        } else {
            false
        }
    }
}

impl<A: Actor> Default for ActivationDirectory<A> {
    fn default() -> Self {
        Self::new()
    }
}

/// Create storage key for an ActorId.
///
/// Format: `namespace::actor_type/key`
///
/// This provides natural isolation:
/// - Different namespaces can't access each other's actors
/// - Different actor types are isolated
fn storage_key(actor_id: &ActorId) -> String {
    format!(
        "{}::{}/{}",
        actor_id.namespace, actor_id.actor_type, actor_id.key
    )
}

/// Actor catalog managing local activations with double-check locking.
///
/// `ActorCatalog` is the central registry for all actors active on this node.
/// It implements the Orleans double-check locking pattern to prevent duplicate
/// actor instances during concurrent activation attempts.
///
/// # Lifecycle States
///
/// ```text
/// [NotExists] → [Creating] → [Activating] → [Valid] → [Deactivating] → [NotExists]
/// ```
///
/// # Double-Check Locking
///
/// The `get_or_create_activation()` method uses double-check locking:
///
/// 1. **Fast path**: Check directory without lock (99% case - actor exists)
/// 2. **Slow path**: Acquire lock, double-check, create if needed (1% case)
///
/// This ensures only one ActorContext is created per ActorId while minimizing
/// lock contention.
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::prelude::*;
/// use moonpool::actor::ActorCatalog;
///
/// // Create catalog for BankAccountActor
/// let node_id = NodeId::from("127.0.0.1:8001")?;
/// let catalog = ActorCatalog::<BankAccountActor>::new(node_id);
///
/// // Get or create activation
/// let actor_id = ActorId::from_string("prod::BankAccount/alice")?;
/// let context = catalog.get_or_create_activation(actor_id.clone())?;
///
/// // Activate (happens outside lock)
/// context.activate().await?;
///
/// // Later: find existing activation
/// if let Some(existing) = catalog.get(actor_id) {
///     println!("Actor is already active");
/// }
///
/// // Deactivate
/// catalog.remove(&actor_id)?;
/// ```
/// Queue size constants for actor channels.
const ACTOR_MESSAGE_QUEUE_SIZE: usize = 128;
const ACTOR_CONTROL_QUEUE_SIZE: usize = 8;

pub struct ActorCatalog<A: Actor, T: moonpool_foundation::TaskProvider> {
    /// Local activation directory (ActorId → ActorContext).
    activation_directory: ActivationDirectory<A>,

    /// Coarse lock for get-or-create operations.
    ///
    /// This lock is only held during the critical section of activation creation
    /// (checking + inserting into the directory). Actual actor activation happens
    /// outside the lock to avoid blocking other activation creation.
    ///
    /// Uses `RefCell<()>` as a simple mutex for single-threaded execution.
    activation_lock: RefCell<()>,

    /// This node's ID.
    node_id: NodeId,

    /// MessageBus reference for message processing.
    ///
    /// Set by ActorRuntime when connecting the catalog to the bus.
    /// Required for actors to send responses.
    message_bus: RefCell<Option<Rc<crate::messaging::MessageBus>>>,

    /// TaskProvider for spawning message loop tasks.
    ///
    /// Generic type parameter allows compile-time dispatch.
    task_provider: T,
}

impl<A: Actor + 'static, T: moonpool_foundation::TaskProvider> ActorCatalog<A, T> {
    /// Create a new ActorCatalog for this node.
    ///
    /// # Parameters
    ///
    /// - `node_id`: The NodeId of this node
    /// - `task_provider`: The TaskProvider for spawning message loop tasks
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let node_id = NodeId::from("127.0.0.1:8001")?;
    /// let task_provider = LocalTaskProvider::new();
    /// let catalog = ActorCatalog::<BankAccountActor, _>::new(node_id, task_provider);
    /// ```
    pub fn new(node_id: NodeId, task_provider: T) -> Self {
        Self {
            activation_directory: ActivationDirectory::new(),
            activation_lock: RefCell::new(()),
            node_id,
            message_bus: RefCell::new(None),
            task_provider,
        }
    }

    /// Set the MessageBus reference for this catalog.
    ///
    /// This is called by ActorRuntime when initializing the system.
    pub fn set_message_bus(&self, message_bus: Rc<crate::messaging::MessageBus>) {
        *self.message_bus.borrow_mut() = Some(message_bus);
    }

    /// Get an existing activation by ActorId.
    ///
    /// Returns `Some(context)` if the actor is active locally, `None` otherwise.
    /// This is a fast, lock-free lookup.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The ActorId to look up
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if let Some(context) = catalog.get(&actor_id) {
    ///     // Actor is active, enqueue message
    ///     context.enqueue_message(message);
    /// } else {
    ///     // Actor not active, create it
    ///     let context = catalog.get_or_create_activation(actor_id)?;
    /// }
    /// ```
    pub fn get(&self, actor_id: &ActorId) -> Option<Rc<ActorContext<A>>> {
        self.activation_directory.find_target(actor_id)
    }

    /// Get or create an activation using double-check locking.
    ///
    /// This method implements the Orleans double-check locking pattern to prevent
    /// duplicate activations while minimizing lock contention:
    ///
    /// 1. **Fast path** (no lock): Check if activation exists → return if found
    /// 2. **Slow path** (with lock): Double-check → create if needed → return
    ///
    /// The returned `ActorContext` is ready to be activated. The caller must
    /// call `context.activate()` **outside** this method to avoid holding the
    /// lock during potentially slow I/O operations.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The ActorId to get or create
    /// - `actor_instance`: The actor implementation to use if creating new activation
    ///
    /// # Returns
    ///
    /// - `Ok(context)`: The ActorContext (either existing or newly created)
    /// - `Err(ActorError)`: If creation fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Create actor instance
    /// let actor = BankAccountActor::new(actor_id.clone());
    ///
    /// // Get or create activation
    /// let context = catalog.get_or_create_activation(actor_id, actor)?;
    ///
    /// // IMPORTANT: Activate outside lock
    /// context.activate().await?;
    ///
    /// // Now ready to process messages
    /// context.enqueue_message(message);
    /// ```
    ///
    /// # Race Handling
    ///
    /// If two concurrent calls to `get_or_create_activation()` occur for the same
    /// ActorId, the double-check pattern ensures only one ActorContext is created:
    ///
    /// ```text
    /// Thread 1                    | Thread 2
    /// ---------------------------|---------------------------
    /// Fast check: None           | Fast check: None
    /// Acquire lock               | Wait for lock...
    /// Double-check: None         |
    /// Create activation          |
    /// Record in directory        |
    /// Release lock               | Acquire lock
    /// Return new context         | Double-check: Some!
    ///                            | Release lock
    ///                            | Return existing context (actor instance discarded)
    /// ```
    pub fn get_or_create_activation(
        &self,
        actor_id: ActorId,
        actor_instance: A,
    ) -> Result<Rc<ActorContext<A>>, ActorError> {
        // FAST PATH: Check without lock (99% case - activation exists)
        if let Some(activation) = self.activation_directory.find_target(&actor_id) {
            return Ok(activation);
        }

        // SLOW PATH: Acquire lock for creation (1% case - first access per actor)
        let _guard = self.activation_lock.borrow_mut();

        // DOUBLE-CHECK under lock (protect against TOCTOU race)
        if let Some(activation) = self.activation_directory.find_target(&actor_id) {
            // Actor instance that was passed in will be dropped (race loser)
            return Ok(activation); // Another task created while we waited
        }

        // Get required dependencies
        let message_bus = self.message_bus.borrow().clone().ok_or_else(|| {
            ActorError::ProcessingFailed("MessageBus not set in ActorCatalog".to_string())
        })?;

        // CREATE CHANNELS (message channel + control channel)
        use tokio::sync::mpsc;
        let (msg_tx, msg_rx) = mpsc::channel::<Message>(ACTOR_MESSAGE_QUEUE_SIZE);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<crate::actor::LifecycleCommand<A>>(ACTOR_CONTROL_QUEUE_SIZE);

        // CREATE ACTOR CONTEXT (while holding lock)
        let context = Rc::new(ActorContext::new(
            actor_id.clone(),
            self.node_id.clone(),
            actor_instance,
            msg_tx,
            ctrl_tx,
        ));

        // Set MessageBus reference on context
        context.set_message_bus(message_bus.clone());

        // SPAWN MESSAGE LOOP TASK (Orleans pattern: spawned once, runs forever until deactivation)
        let ctx_clone = context.clone();
        let bus_clone = message_bus.clone();

        let task_name = format!("actor_loop_{}", actor_id);
        let task_handle = self.task_provider.spawn_task(&task_name, async move {
            crate::actor::run_message_loop(ctx_clone, msg_rx, ctrl_rx, bus_clone).await
        });

        // Store task handle in context
        context.set_message_loop_task(task_handle);

        // REGISTER IN DIRECTORY
        self.activation_directory.record_new_target(context.clone());

        // Return for activation outside lock
        Ok(context)
    }

    /// Check if an activation exists.
    ///
    /// Returns `true` if the actor is active locally, `false` otherwise.
    /// This is equivalent to `self.get(actor_id).is_some()` but more readable.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The ActorId to check
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if catalog.exists(&actor_id) {
    ///     println!("Actor is already active");
    /// } else {
    ///     println!("Actor needs activation");
    /// }
    /// ```
    pub fn exists(&self, actor_id: &ActorId) -> bool {
        self.activation_directory.find_target(actor_id).is_some()
    }

    /// Remove an activation from the catalog.
    ///
    /// This is called during actor deactivation to remove the ActorContext from
    /// the directory. Returns `Ok(())` if removed, `Err` if not found.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The ActorId to remove
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Activation was removed
    /// - `Err(ActorError::NotFound)`: Activation was not in the catalog
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Deactivate actor
    /// context.deactivate(DeactivationReason::ApplicationRequested).await?;
    ///
    /// // Remove from catalog
    /// catalog.remove(&actor_id)?;
    /// ```
    pub fn remove(&self, actor_id: &ActorId) -> Result<(), ActorError> {
        if self.activation_directory.remove_target(actor_id) {
            Ok(())
        } else {
            Err(ActorError::NotFound(format!(
                "Activation not found for actor: {}",
                actor_id
            )))
        }
    }

    /// Get the number of active activations.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let count = catalog.count();
    /// println!("Active actors: {}", count);
    /// ```
    pub fn count(&self) -> usize {
        self.activation_directory.count()
    }
}

// Implement ActorRouter trait for ActorCatalog
use crate::messaging::{ActorRouter, Message};
use async_trait::async_trait;

#[async_trait(?Send)]
impl<A: Actor + 'static, T: moonpool_foundation::TaskProvider> ActorRouter for ActorCatalog<A, T> {
    /// Route a message to the appropriate local actor.
    ///
    /// This implementation:
    /// 1. Gets or creates the actor activation (with double-check locking)
    /// 2. Enqueues the message in the actor's message queue
    /// 3. Spawns message processing task if not already running
    ///
    /// # Parameters
    ///
    /// - `message`: The message to route to an actor
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Message successfully routed to actor's queue
    /// - `Err(ActorError)`: Routing failed (activation failed, etc.)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use moonpool::messaging::ActorRouter;
    ///
    /// let catalog = ActorCatalog::<BankAccountActor>::new(node_id);
    /// let router: Rc<dyn ActorRouter> = Rc::new(catalog);
    ///
    /// // Route message to actor
    /// router.route_message(message).await?;
    /// ```
    async fn route_message(&self, message: Message) -> Result<(), ActorError> {
        // Get the actor context
        // TODO: In Phase 4, we'll have an ActorFactory trait to auto-create actors.
        // For now, return NotFound if actor doesn't exist.
        let context = self.get(&message.target_actor).ok_or_else(|| {
            ActorError::NotFound(format!(
                "Actor not found (and auto-activation not yet implemented): {}",
                message.target_actor
            ))
        })?;

        // Enqueue message in actor's channel (async send)
        // This automatically wakes the message loop task
        context.enqueue_message(message).await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::{ActorId, NodeId};
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};
    use moonpool_foundation::TokioTaskProvider;

    // Dummy actor type for testing
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct DummyActor;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct DummyState;

    #[async_trait(?Send)]
    impl Actor for DummyActor {
        type State = DummyState;

        fn actor_id(&self) -> &ActorId {
            unimplemented!("Not used in catalog tests")
        }

        async fn on_activate(
            &mut self,
            _state: Option<Self::State>,
        ) -> Result<(), crate::error::ActorError> {
            Ok(())
        }

        async fn on_deactivate(
            &mut self,
            _reason: crate::actor::DeactivationReason,
        ) -> Result<(), crate::error::ActorError> {
            Ok(())
        }
    }


    #[test]
    fn test_activation_directory_basic_operations() {
        use tokio::sync::mpsc;

        let directory = ActivationDirectory::<DummyActor>::new();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();

        // Initially empty
        assert_eq!(directory.count(), 0);

        // Create channels and record activation
        let actor_id = ActorId::from_string("test::Counter/alice").unwrap();
        let (msg_tx, _msg_rx) = mpsc::channel(128);
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);

        let context = Rc::new(ActorContext::new(
            actor_id.clone(),
            node_id.clone(),
            DummyActor,
            msg_tx,
            ctrl_tx,
        ));
        directory.record_new_target(context.clone());

        // Should be found
        assert_eq!(directory.count(), 1);
        assert!(directory.find_target(&actor_id).is_some());

        // Remove activation
        assert!(directory.remove_target(&actor_id));
        assert_eq!(directory.count(), 0);
        assert!(directory.find_target(&actor_id).is_none());

        // Remove again (idempotent failure)
        assert!(!directory.remove_target(&actor_id));
    }

    #[test]
    fn test_activation_directory_storage_key_isolation() {
        use tokio::sync::mpsc;

        let directory = ActivationDirectory::<DummyActor>::new();
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();

        // Different namespaces
        let prod_actor = ActorId::from_string("prod::Counter/alice").unwrap();
        let staging_actor = ActorId::from_string("staging::Counter/alice").unwrap();

        let (msg_tx1, _) = mpsc::channel(128);
        let (ctrl_tx1, _) = mpsc::channel(8);
        let (msg_tx2, _) = mpsc::channel(128);
        let (ctrl_tx2, _) = mpsc::channel(8);

        let prod_context = Rc::new(ActorContext::new(
            prod_actor.clone(),
            node_id.clone(),
            DummyActor,
            msg_tx1,
            ctrl_tx1,
        ));
        let staging_context = Rc::new(ActorContext::new(
            staging_actor.clone(),
            node_id.clone(),
            DummyActor,
            msg_tx2,
            ctrl_tx2,
        ));

        directory.record_new_target(prod_context);
        directory.record_new_target(staging_context);

        // Should be isolated
        assert_eq!(directory.count(), 2);
        assert!(directory.find_target(&prod_actor).is_some());
        assert!(directory.find_target(&staging_actor).is_some());

        // Different actor types
        let counter_actor = ActorId::from_string("prod::Counter/bob").unwrap();
        let account_actor = ActorId::from_string("prod::BankAccount/bob").unwrap();

        let (msg_tx3, _) = mpsc::channel(128);
        let (ctrl_tx3, _) = mpsc::channel(8);
        let (msg_tx4, _) = mpsc::channel(128);
        let (ctrl_tx4, _) = mpsc::channel(8);

        let counter_context = Rc::new(ActorContext::new(
            counter_actor.clone(),
            node_id.clone(),
            DummyActor,
            msg_tx3,
            ctrl_tx3,
        ));
        let account_context = Rc::new(ActorContext::new(
            account_actor.clone(),
            node_id,
            DummyActor,
            msg_tx4,
            ctrl_tx4,
        ));

        directory.record_new_target(counter_context);
        directory.record_new_target(account_context);

        // Should be isolated
        assert_eq!(directory.count(), 4);
        assert!(directory.find_target(&counter_actor).is_some());
        assert!(directory.find_target(&account_actor).is_some());
    }

    #[test]
    fn test_actor_catalog_get_or_create() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let task_provider = TokioTaskProvider;
            let catalog = ActorCatalog::<DummyActor, _>::new(node_id.clone(), task_provider);

            // Set MessageBus (required for spawning message loop task)
            let message_bus = Rc::new(crate::messaging::MessageBus::new(node_id));
            catalog.set_message_bus(message_bus);

            let actor_id = ActorId::from_string("test::Counter/charlie").unwrap();

        // Initially not exists
        assert!(!catalog.exists(&actor_id));
        assert_eq!(catalog.count(), 0);

        // Get or create (first time - creates)
        let context1 = catalog
            .get_or_create_activation(actor_id.clone(), DummyActor)
            .unwrap();
        assert_eq!(catalog.count(), 1);
        assert!(catalog.exists(&actor_id));

        // Get or create (second time - returns existing)
        let context2 = catalog
            .get_or_create_activation(actor_id.clone(), DummyActor)
            .unwrap();
        assert_eq!(catalog.count(), 1); // Still 1, not 2

        // Same instance
        assert!(Rc::ptr_eq(&context1, &context2));

        // Get (without create)
        let context3 = catalog.get(&actor_id).unwrap();
        assert!(Rc::ptr_eq(&context1, &context3));

        // Remove
        catalog.remove(&actor_id).unwrap();
        assert_eq!(catalog.count(), 0);
        assert!(!catalog.exists(&actor_id));

            // Remove again (should fail)
            assert!(catalog.remove(&actor_id).is_err());
        });
    }

    #[test]
    fn test_actor_catalog_double_check_prevents_duplicates() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let task_provider = TokioTaskProvider;
            let catalog = ActorCatalog::<DummyActor, _>::new(node_id.clone(), task_provider);

            // Set MessageBus (required for spawning message loop task)
            let message_bus = Rc::new(crate::messaging::MessageBus::new(node_id));
            catalog.set_message_bus(message_bus);

            let actor_id = ActorId::from_string("test::Counter/dave").unwrap();

        // Simulate concurrent calls (single-threaded simulation)
        let context1 = catalog
            .get_or_create_activation(actor_id.clone(), DummyActor)
            .unwrap();
        let context2 = catalog
            .get_or_create_activation(actor_id.clone(), DummyActor)
            .unwrap();
        let context3 = catalog
            .get_or_create_activation(actor_id, DummyActor)
            .unwrap();

        // All should be the same instance
        assert!(Rc::ptr_eq(&context1, &context2));
        assert!(Rc::ptr_eq(&context1, &context3));

            // Only one activation created
            assert_eq!(catalog.count(), 1);
        });
    }

    #[test]
    fn test_actor_catalog_multiple_actors() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let task_provider = TokioTaskProvider;
            let catalog = ActorCatalog::<DummyActor, _>::new(node_id.clone(), task_provider);

            // Set MessageBus (required for spawning message loop task)
            let message_bus = Rc::new(crate::messaging::MessageBus::new(node_id));
            catalog.set_message_bus(message_bus);

        // Create multiple actors
        let alice = ActorId::from_string("test::Counter/alice").unwrap();
        let bob = ActorId::from_string("test::Counter/bob").unwrap();
        let charlie = ActorId::from_string("test::BankAccount/charlie").unwrap();

        let context_alice = catalog
            .get_or_create_activation(alice.clone(), DummyActor)
            .unwrap();
        let context_bob = catalog
            .get_or_create_activation(bob.clone(), DummyActor)
            .unwrap();
        let context_charlie = catalog
            .get_or_create_activation(charlie.clone(), DummyActor)
            .unwrap();

        // All should be different instances
        assert!(!Rc::ptr_eq(&context_alice, &context_bob));
        assert!(!Rc::ptr_eq(&context_alice, &context_charlie));
        assert!(!Rc::ptr_eq(&context_bob, &context_charlie));

        // Count should be 3
        assert_eq!(catalog.count(), 3);

        // Remove one
            catalog.remove(&bob).unwrap();
            assert_eq!(catalog.count(), 2);
            assert!(!catalog.exists(&bob));
            assert!(catalog.exists(&alice));
            assert!(catalog.exists(&charlie));
        });
    }
}
