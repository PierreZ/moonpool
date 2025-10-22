//! Actor runtime - main entry point for actor system.

use crate::actor::{Actor, ActorId, NodeId};
use crate::error::ActorError;
use crate::runtime::ActorRuntimeBuilder;
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
/// // Get actor reference
/// let actor = runtime.get_actor::<BankAccountActor>("BankAccount", "alice");
///
/// // Send message
/// let balance = actor.call(DepositRequest { amount: 100 }).await?;
///
/// // Shutdown
/// runtime.shutdown(Duration::from_secs(30)).await?;
/// ```
pub struct ActorRuntime {
    /// Cluster namespace (all actors in this runtime share this namespace).
    namespace: String,

    /// This node's identifier.
    node_id: NodeId,

    // TODO: Add fields as we integrate components
    // catalog: Rc<ActorCatalog<A>>,  // Challenge: Generic over all actor types
    // directory: Rc<SimpleDirectory>,
    // message_bus: Rc<MessageBus>,
}

impl ActorRuntime {
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
    pub fn builder() -> ActorRuntimeBuilder {
        ActorRuntimeBuilder::new()
    }

    /// Create a new ActorRuntime (internal, used by builder).
    pub(crate) fn new(namespace: String, node_id: NodeId) -> Result<Self, ActorError> {
        Ok(Self {
            namespace,
            node_id,
        })
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
    /// let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice");
    /// // Creates ActorId: "prod::BankAccount/alice"
    ///
    /// let bob = runtime.get_actor::<BankAccountActor>("BankAccount", "bob");
    /// // Creates ActorId: "prod::BankAccount/bob"
    /// ```
    pub fn get_actor<A: Actor>(
        &self,
        actor_type: impl Into<String>,
        key: impl Into<String>,
    ) -> Result<ActorId, ActorError> {
        // Create ActorId with runtime's namespace
        let actor_id = ActorId::from_parts(
            self.namespace.clone(),
            actor_type.into(),
            key.into(),
        )?;

        Ok(actor_id)

        // TODO: Return ActorRef<A> instead of ActorId
        // This requires implementing ActorRef in next task (T065-T068)
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
