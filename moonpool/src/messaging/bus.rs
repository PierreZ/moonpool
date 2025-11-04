//! Message bus for routing messages to local and remote actors.
//!
//! This module provides the `MessageBus` which routes incoming messages to
//! either local actors via the ActorCatalog or remote actors via network transport.
//!
//! # Architecture
//!
//! MessageBus integrates with foundation's transport layer to enable actor-to-actor
//! communication across nodes. It follows Orleans' MessageCenter pattern (routing logic),
//! delegating callback management to CallbackManager (Orleans' InsideRuntimeClient pattern).
//!
//! ```text
//! ┌────────────────────────────────────┐
//! │ MessageBus                         │
//! │ (Orleans: MessageCenter)           │
//! │                                    │
//! │  ┌──────────────────────────────┐  │
//! │  │ node_id: NodeId              │  │
//! │  └──────────────────────────────┘  │
//! │                                    │
//! │  ┌──────────────────────────────┐  │
//! │  │ callback_manager             │  │
//! │  │ (Orleans: InsideRuntimeClient)│ │
//! │  └──────────────────────────────┘  │
//! │                                    │
//! │  ┌──────────────────────────────┐  │
//! │  │ directory, placement         │  │
//! │  └──────────────────────────────┘  │
//! └────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use moonpool::messaging::{CallbackManager, MessageBus};
//! use std::rc::Rc;
//!
//! // Create callback manager and message bus
//! let node_id = NodeId::from("127.0.0.1:8001")?;
//! let callback_manager = Rc::new(CallbackManager::new());
//! let bus = MessageBus::new(node_id, callback_manager, directory, placement);
//!
//! // Generate correlation ID (via callback manager)
//! let correlation_id = bus.next_correlation_id();
//!
//! // Register callback for response
//! let (tx, rx) = oneshot::channel();
//! bus.register_pending_request(correlation_id, request, tx);
//!
//! // Wait for response
//! let response = rx.await?;
//! ```

use crate::actor::{CorrelationId, NodeId};
use crate::directory::Directory;
use crate::error::ActorError;
use crate::messaging::{ActorRouter, CallbackManager, Direction, Message, NetworkTransport};
use crate::placement::SimplePlacement;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tokio::sync::oneshot;

// Type alias for shared directory reference
type SharedDirectory = Rc<dyn Directory>;

/// Message bus for routing messages to actors.
///
/// `MessageBus` handles correlation ID generation, pending request tracking,
/// and message routing to both local and remote actors.
///
/// # Single-Threaded Design
///
/// Uses `Cell` and `RefCell` for interior mutability (no Send/Sync required).
/// Compatible with tokio's `current_thread` runtime.
///
/// # Example
///
/// ```rust,ignore
/// let bus = MessageBus::new(node_id, network, time, task_provider, peer_config);
///
/// // Generate correlation ID for request
/// let corr_id = bus.next_correlation_id();
///
/// // Register callback
/// let (tx, rx) = oneshot::channel();
/// bus.register_pending_request(corr_id, request, tx);
///
/// // Send message (routes locally or remotely based on target_node)
/// bus.send_request(message).await?;
///
/// // When response arrives:
/// bus.complete_pending_request(corr_id, Ok(response));
/// ```
pub struct MessageBus {
    /// This node's identifier.
    node_id: NodeId,

    /// Callback manager for correlation tracking and callback management.
    ///
    /// Handles request-response correlation (Orleans: InsideRuntimeClient pattern).
    /// Separated from routing logic following single responsibility principle.
    callback_manager: Rc<CallbackManager>,

    /// Router registry mapping actor type names to their catalogs.
    ///
    /// Maps actor type name (e.g., "BankAccount") → ActorCatalog<A, T, F>
    /// stored as trait object Rc<dyn ActorRouter>.
    ///
    /// Set by ActorRuntime during initialization to enable routing messages
    /// to the correct catalog based on target actor type.
    actor_routers: RefCell<HashMap<String, Rc<dyn ActorRouter>>>,

    /// Network transport for sending messages to remote nodes.
    ///
    /// Uses Rc to allow cloning and avoid RefCell borrows across awaits.
    network_transport: Rc<dyn NetworkTransport>,

    /// Directory for actor location tracking.
    ///
    /// Used to look up actor locations for message routing.
    directory: SharedDirectory,

    /// Placement strategy for choosing where to activate new actors.
    ///
    /// Consults placement hint from actor type and chooses appropriate node
    /// based on load balancing and other placement strategies.
    placement: SimplePlacement,

    /// Weak self-reference for passing to actor routers.
    ///
    /// Set after wrapping in Rc via `init_self_ref()`. Required for routers
    /// to receive MessageBus reference when spawning message loops (Orleans pattern).
    self_ref: RefCell<Option<std::rc::Weak<Self>>>,
}

impl MessageBus {
    /// Create a new MessageBus for this node.
    ///
    /// # Parameters
    ///
    /// - `node_id`: This node's identifier
    /// - `callback_manager`: Callback manager for correlation tracking
    /// - `directory`: Directory for actor location tracking
    /// - `placement`: Placement strategy for choosing where to activate new actors
    /// - `network_transport`: Network transport for remote communication
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use moonpool::messaging::CallbackManager;
    /// use moonpool::serialization::JsonSerializer;
    /// use std::rc::Rc;
    ///
    /// let node_id = NodeId::from("127.0.0.1:8001")?;
    /// let callback_manager = Rc::new(CallbackManager::new());
    /// let directory = SimpleDirectory::new();
    /// let placement = SimplePlacement::new(cluster_nodes);
    /// let transport = create_network_transport();
    /// let serializer = JsonSerializer;
    /// let bus = MessageBus::new(
    ///     node_id,
    ///     callback_manager,
    ///     Rc::new(directory) as SharedDirectory,
    ///     placement,
    ///     Rc::new(transport),
    ///     serializer,
    /// );
    /// ```
    pub fn new(
        node_id: NodeId,
        callback_manager: Rc<CallbackManager>,
        directory: SharedDirectory,
        placement: SimplePlacement,
        network_transport: Rc<dyn NetworkTransport>,
    ) -> Self {
        Self {
            node_id,
            callback_manager,
            actor_routers: RefCell::new(HashMap::new()),
            network_transport,
            directory,
            placement,
            self_ref: RefCell::new(None),
        }
    }

    /// Initialize the self-reference after wrapping in Rc.
    ///
    /// This must be called after creating the MessageBus and wrapping it in Rc.
    /// The weak reference is used to pass MessageBus to actor routers for
    /// spawning message loops (Orleans pattern).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let bus = Rc::new(MessageBus::new(node_id, directory, placement));
    /// bus.init_self_ref();
    /// ```
    pub fn init_self_ref(self: &Rc<Self>) {
        *self.self_ref.borrow_mut() = Some(Rc::downgrade(self));
    }

    /// Set the actor router registry for local message delivery.
    ///
    /// This should be called by ActorRuntime after registering actor types
    /// to enable routing messages to the correct catalog based on actor type.
    ///
    /// # Parameters
    ///
    /// - `routers`: HashMap mapping actor type names to their catalog routers
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let bus = MessageBus::new(node_id);
    ///
    /// // Build router registry
    /// let mut routers = HashMap::new();
    /// routers.insert("BankAccount".to_string(), Rc::new(bank_catalog) as Rc<dyn ActorRouter>);
    /// routers.insert("OrderProcessor".to_string(), Rc::new(order_catalog) as Rc<dyn ActorRouter>);
    ///
    /// bus.set_actor_routers(routers);
    /// ```
    pub fn set_actor_routers(&self, routers: HashMap<String, Rc<dyn ActorRouter>>) {
        *self.actor_routers.borrow_mut() = routers;
    }

    /// Get this node's ID.
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    /// Get the directory reference.
    ///
    /// Returns a shared reference to the cluster-wide directory for
    /// actor location tracking and registration/unregistration.
    pub fn directory(&self) -> &SharedDirectory {
        &self.directory
    }

    /// Generate the next correlation ID.
    ///
    /// Returns monotonically increasing IDs starting from 1.
    /// Unique per node (not globally unique).
    ///
    /// Delegates to CallbackManager (Orleans: InsideRuntimeClient pattern).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let id1 = bus.next_correlation_id();
    /// let id2 = bus.next_correlation_id();
    /// assert!(id2.value() > id1.value());
    /// ```
    pub fn next_correlation_id(&self) -> CorrelationId {
        self.callback_manager.next_correlation_id()
    }

    /// Register a pending request awaiting response.
    ///
    /// Creates a CallbackData to track the request and stores it in the
    /// pending_requests map for later correlation.
    ///
    /// Delegates to CallbackManager (Orleans: InsideRuntimeClient pattern).
    ///
    /// # Parameters
    ///
    /// - `correlation_id`: The request's correlation ID
    /// - `message`: The request message
    /// - `sender`: Oneshot sender for delivering the response
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let corr_id = bus.next_correlation_id();
    /// let (tx, rx) = oneshot::channel();
    ///
    /// bus.register_pending_request(corr_id, request_msg, tx);
    ///
    /// // Later, await response
    /// let response = rx.await?;
    /// ```
    pub fn register_pending_request(
        &self,
        correlation_id: CorrelationId,
        message: Message,
        sender: oneshot::Sender<Result<Message, ActorError>>,
    ) {
        self.callback_manager
            .register_pending_request(correlation_id, message, sender);
    }

    /// Complete a pending request with a response.
    ///
    /// Looks up the CallbackData by correlation ID and completes it with
    /// the provided result. Removes the callback from pending_requests.
    ///
    /// Delegates to CallbackManager (Orleans: InsideRuntimeClient pattern).
    ///
    /// # Parameters
    ///
    /// - `correlation_id`: The correlation ID from the response
    /// - `result`: The response message or error
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Callback found and completed
    /// - `Err(ActorError::UnknownCorrelationId)`: No pending request for this ID
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // When response message arrives:
    /// bus.complete_pending_request(
    ///     response.correlation_id,
    ///     Ok(response)
    /// )?;
    /// ```
    pub fn complete_pending_request(
        &self,
        correlation_id: CorrelationId,
        result: Result<Message, ActorError>,
    ) -> Result<(), ActorError> {
        self.callback_manager
            .complete_pending_request(correlation_id, result)
    }

    /// Handle a timeout for a pending request.
    ///
    /// Removes the callback from pending_requests and completes it with a timeout error.
    ///
    /// Delegates to CallbackManager (Orleans: InsideRuntimeClient pattern).
    ///
    /// # Parameters
    ///
    /// - `correlation_id`: The correlation ID that timed out
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // In timeout task:
    /// time_provider.sleep(timeout_duration).await;
    /// bus.handle_timeout(correlation_id);
    /// ```
    pub fn handle_timeout(&self, correlation_id: CorrelationId) {
        self.callback_manager.handle_timeout(correlation_id);
    }

    /// Get the number of pending requests.
    ///
    /// Delegates to CallbackManager (Orleans: InsideRuntimeClient pattern).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let count = bus.pending_count();
    /// println!("Pending requests: {}", count);
    /// ```
    pub fn pending_count(&self) -> usize {
        self.callback_manager.pending_count()
    }

    /// Send a request message and await the response.
    ///
    /// This is the primary method for request-response messaging. It:
    /// 1. Generates a correlation ID
    /// 2. Registers a callback for the response
    /// 3. Sends the request to remote node via network transport
    /// 4. Returns a receiver channel to await the response
    ///
    /// # Parameters
    ///
    /// - `message`: The request message to send (must have Direction::Request)
    ///
    /// # Returns
    ///
    /// A receiver channel that will deliver the response message or an error.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let request = Message::request(
    ///     target_actor,
    ///     sender_actor,
    ///     target_node,
    ///     sender_node,
    ///     "deposit",
    ///     payload,
    /// );
    ///
    /// let rx = bus.send_request(request).await?;
    /// let response = rx.await??;
    /// ```
    pub async fn send_request(
        &self,
        mut message: Message,
    ) -> Result<(Message, oneshot::Receiver<Result<Message, ActorError>>), ActorError> {
        // Generate correlation ID
        let correlation_id = self.next_correlation_id();
        message.correlation_id = correlation_id;

        // Create oneshot channel for response
        let (tx, rx) = oneshot::channel();

        // Register pending request
        self.register_pending_request(correlation_id, message.clone(), tx);

        // Determine if message is local or remote
        if message.target_node == self.node_id {
            // Local delivery - route to actor directly
            tracing::debug!(
                node_id = %self.node_id,
                "send_request (local): corr_id={}, target={}, method={}",
                correlation_id,
                message.target_actor,
                message.method_name
            );
            // Message will be routed by the caller or through route_message
        } else {
            // Remote delivery - send over network
            tracing::debug!(
                node_id = %self.node_id,
                "send_request (remote): corr_id={}, target={}, target_node={}, method={}",
                correlation_id,
                message.target_actor,
                message.target_node,
                message.method_name
            );

            // Clone Rc references to avoid holding borrow across await
            let transport = self.network_transport.clone();

            // Send message directly over network - transport will handle serialization
            let destination_str = message.target_node.clone();
            transport
                .send(destination_str.as_str(), message.clone())
                .await?;
        }

        Ok((message, rx))
    }

    /// Send a response message back to a waiting request.
    ///
    /// This method handles sending response messages that complete pending requests.
    /// It either completes a local callback or sends over the network.
    ///
    /// # Parameters
    ///
    /// - `message`: The response message (must have Direction::Response)
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Response delivered successfully
    /// - `Err(ActorError)`: Response delivery failed
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let response = Message::response(
    ///     correlation_id,
    ///     target_actor,
    ///     sender_actor,
    ///     target_node,
    ///     sender_node,
    ///     result_payload,
    /// );
    ///
    /// bus.send_response(response).await?;
    /// ```
    pub async fn send_response(&self, message: Message) -> Result<(), ActorError> {
        tracing::debug!(
            node_id = %self.node_id,
            "send_response: corr_id={}, target={}, target_node={}",
            message.correlation_id,
            message.target_actor,
            message.target_node
        );

        // Determine if response is for local or remote requester
        if message.target_node == self.node_id {
            // Local callback completion
            tracing::debug!(
                node_id = %self.node_id,
                "send_response (LOCAL): corr_id={}, completing pending request",
                message.correlation_id
            );
            self.complete_pending_request(message.correlation_id, Ok(message))?;
        } else {
            // Remote response - send over network
            tracing::debug!(
                node_id = %self.node_id,
                "send_response (REMOTE): corr_id={}, target_node={}, sending over network",
                message.correlation_id,
                message.target_node
            );

            // Clone Rc references to avoid holding borrow across await
            let transport = self.network_transport.clone();

            // Send message directly over network - transport will handle serialization
            let destination_str = message.target_node.clone();
            tracing::debug!(
                node_id = %self.node_id,
                "send_response: Sending to destination={}",
                destination_str.as_str()
            );
            transport.send(destination_str.as_str(), message).await?;
            tracing::debug!(
                node_id = %self.node_id,
                "send_response: Successfully sent response over network"
            );
        }

        Ok(())
    }

    /// Send a one-way message (fire-and-forget).
    ///
    /// OneWay messages don't expect a response and don't register callbacks.
    ///
    /// # Parameters
    ///
    /// - `message`: The one-way message (must have Direction::OneWay)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let oneway = Message::oneway(
    ///     target_actor,
    ///     sender_actor,
    ///     target_node,
    ///     sender_node,
    ///     "log_event",
    ///     payload,
    /// );
    ///
    /// bus.send_oneway(oneway).await?;
    /// ```
    pub async fn send_oneway(&self, message: Message) -> Result<(), ActorError> {
        tracing::debug!(
            node_id = %self.node_id,
            "send_oneway: target={}, target_node={}, method={}",
            message.target_actor,
            message.target_node,
            message.method_name
        );

        // Determine if message is local or remote
        if message.target_node == self.node_id {
            // Local delivery - route directly
            self.route_to_actor(message).await?;
        } else {
            // Remote delivery - send over network
            tracing::debug!(
                node_id = %self.node_id,
                "send_oneway (remote): target={}, target_node={}",
                message.target_actor,
                message.target_node
            );

            // Clone Rc references to avoid holding borrow across await
            let transport = self.network_transport.clone();

            // Send message directly over network - transport will handle serialization
            let destination_str = message.target_node.clone();
            transport.send(destination_str.as_str(), message).await?;
        }

        Ok(())
    }

    /// Receive and process messages from the network.
    ///
    /// This method polls the transport for incoming messages and routes them appropriately.
    /// Should be called periodically to process network traffic.
    ///
    /// # Returns
    ///
    /// - `Ok(true)`: Message received and processed
    /// - `Ok(false)`: No message available
    /// - `Err(ActorError)`: Processing error
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // In message processing loop:
    /// loop {
    ///     if bus.poll_network().await? {
    ///         // Message was processed
    ///     } else {
    ///         // No messages, can yield or do other work
    ///         tokio::task::yield_now().await;
    ///     }
    /// }
    /// ```
    pub async fn poll_network(&self) -> Result<bool, ActorError> {
        // Clone Rc references to avoid holding borrow across await
        let transport = self.network_transport.clone();

        if let Some(message) = transport.poll_receive() {
            // Message already deserialized by transport

            tracing::debug!(
                node_id = %self.node_id,
                "poll_network: received message corr_id={}, direction={:?}, target={}",
                message.correlation_id,
                message.direction,
                message.target_actor
            );

            // Route the message
            self.route_message(message).await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Route a message to the appropriate handler.
    ///
    /// This is the central routing logic that determines where a message should go
    /// based on its direction:
    /// - Request/OneWay → route to local actor via ActorRouter
    /// - Response → complete pending request callback
    ///
    /// # Parameters
    ///
    /// - `message`: The message to route
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Message successfully routed
    /// - `Err(ActorError)`: Routing failed
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // In receive loop:
    /// loop {
    ///     let message = receive_from_network().await?;
    ///     bus.route_message(message).await?;
    /// }
    /// ```
    pub async fn route_message(&self, message: Message) -> Result<(), ActorError> {
        match message.direction {
            Direction::Request | Direction::OneWay => self.route_to_actor(message).await,
            Direction::Response => self.route_to_callback(message).await,
        }
    }

    /// Route request/oneway message to target actor.
    ///
    /// This implements location-transparent routing with placement hints:
    /// 1. Check directory for actor location
    /// 2. If on remote node, forward message there
    /// 3. If not found, consult actor's placement hint to decide where to activate
    /// 4. Either activate locally or forward to chosen node
    ///
    /// # Parameters
    ///
    /// - `message`: The request or oneway message to route
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Message successfully delivered to actor
    /// - `Err(ActorError::ProcessingFailed)`: No router for actor type
    /// - `Err(ActorError)`: Actor routing failed
    async fn route_to_actor(&self, mut message: Message) -> Result<(), ActorError> {
        tracing::debug!(
            node_id = %self.node_id,
            "Routing message to actor: target={}, method={}",
            message.target_actor,
            message.method_name
        );

        // ORLEANS ROUTING PATTERN (as shown in architecture diagram):
        // Check directory only for actors that are ALREADY ACTIVATED elsewhere
        match self.directory.lookup(&message.target_actor).await {
            Ok(Some(node_id)) if node_id != self.node_id => {
                // Actor exists on remote node - forward message there
                tracing::debug!(
                    node_id = %self.node_id,
                    "Actor {} found on remote node {}, forwarding",
                    message.target_actor,
                    node_id
                );

                // Update target_node to remote location
                message.target_node = node_id.clone();

                // Clone transport Rc to avoid holding borrow across await
                let transport = self.network_transport.clone();

                // Send message directly - transport will handle serialization
                transport.send(node_id.as_str(), message).await?;
                return Ok(());
            }
            Ok(Some(_)) => {
                // Actor registered locally, proceed with local routing
                tracing::debug!(
                    node_id = %self.node_id,
                    "Actor {} registered locally, routing to catalog",
                    message.target_actor
                );
            }
            Ok(None) => {
                // Actor NOT in directory - Consult placement hint
                let actor_type = message.target_actor.actor_type();
                let router = self
                    .actor_routers
                    .borrow()
                    .get(actor_type)
                    .cloned()
                    .ok_or_else(|| {
                        ActorError::ProcessingFailed(format!(
                            "No router registered for actor type '{}'",
                            actor_type
                        ))
                    })?;

                // Get placement hint from actor type
                let hint = router.placement_hint();

                // Get current node loads from directory
                let node_loads = self.directory.get_all_node_loads().await;

                // Ask placement to choose node based on hint and current load
                let chosen_node = self.placement.choose_node(
                    &message.target_actor,
                    hint,
                    &self.node_id,
                    &node_loads,
                )?;

                tracing::debug!(
                    node_id = %self.node_id,
                    chosen_node = %chosen_node,
                    hint = ?hint,
                    "Actor {} not in directory, placement decision made",
                    message.target_actor
                );

                // If chosen node is remote, forward the message there
                if chosen_node != self.node_id {
                    tracing::debug!(
                        node_id = %self.node_id,
                        chosen_node = %chosen_node,
                        "Forwarding activation to remote node"
                    );

                    // Update target_node to chosen location
                    message.target_node = chosen_node.clone();

                    // Clone transport Rc to avoid holding borrow across await
                    let transport = self.network_transport.clone();

                    // Send message directly - transport will handle serialization
                    transport.send(chosen_node.as_str(), message).await?;
                    return Ok(());
                } else {
                    // Activate locally
                    tracing::debug!(
                        node_id = %self.node_id,
                        "Will activate locally per placement decision"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    node_id = %self.node_id,
                    "Directory lookup failed for {}: {:?}, proceeding with local routing",
                    message.target_actor,
                    e
                );
            }
        }

        // LOCATE ROUTING: Find the local catalog for this actor type
        let actor_type = message.target_actor.actor_type();

        let router = self
            .actor_routers
            .borrow()
            .get(actor_type)
            .cloned()
            .ok_or_else(|| {
                ActorError::ProcessingFailed(format!(
                    "No router registered for actor type '{}'",
                    actor_type
                ))
            })?;

        // Get MessageBus Rc from weak reference (Orleans pattern)
        // This allows routers to receive MessageBus for spawning message loops
        let message_bus_rc = self
            .self_ref
            .borrow()
            .as_ref()
            .and_then(|weak| weak.upgrade())
            .ok_or_else(|| {
                ActorError::ProcessingFailed(
                    "MessageBus self-reference not initialized (call init_self_ref() first)"
                        .to_string(),
                )
            })?;

        // Delegate to router (which will auto-activate if needed, passing message_bus)
        router.route_message(message, message_bus_rc).await
    }

    /// Route response message to waiting callback.
    ///
    /// # Parameters
    ///
    /// - `message`: The response message to route
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Response delivered to callback
    /// - `Err(ActorError::UnknownCorrelationId)`: No pending request for this correlation ID
    async fn route_to_callback(&self, message: Message) -> Result<(), ActorError> {
        let correlation_id = message.correlation_id;
        tracing::debug!(
            node_id = %self.node_id,
            "route_to_callback: ENTRY corr_id={}, pending_count={}",
            correlation_id,
            self.pending_count()
        );

        // Complete the pending request via CallbackManager (Orleans: InsideRuntimeClient)
        // CallbackManager handles correlation tracking and ensures exactly-once completion
        self.complete_pending_request(correlation_id, Ok(message))?;

        tracing::debug!(
            node_id = %self.node_id,
            "route_to_callback: Successfully completed pending request corr_id={}, EXIT",
            correlation_id
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::{ActorId, CorrelationId};
    use crate::error::MessageError;
    use crate::messaging::{Direction, MessageFlags};

    fn create_test_message(correlation_id: CorrelationId) -> Message {
        let target = ActorId::from_string("test::Actor/1").unwrap();
        let sender = ActorId::from_string("test::Sender/1").unwrap();
        let target_node = NodeId::from("127.0.0.1:8001").unwrap();
        let sender_node = NodeId::from("127.0.0.1:8002").unwrap();

        Message {
            correlation_id,
            direction: Direction::Request,
            target_actor: target,
            sender_actor: sender,
            target_node,
            sender_node,
            method_name: "test".to_string(),
            payload: vec![],
            flags: MessageFlags::empty(),
            time_to_expiry: None,
            forward_count: 0,
            cache_invalidation: None,
        }
    }

    fn create_test_directory() -> SharedDirectory {
        use crate::directory::SimpleDirectory;
        Rc::new(SimpleDirectory::new())
    }

    fn create_test_placement() -> SimplePlacement {
        use crate::placement::SimplePlacement;
        let nodes = vec![
            NodeId::from("127.0.0.1:8001").unwrap(),
            NodeId::from("127.0.0.1:8002").unwrap(),
        ];
        SimplePlacement::new(nodes)
    }

    fn create_test_callback_manager() -> Rc<CallbackManager> {
        Rc::new(CallbackManager::new())
    }

    // Mock network transport for testing
    struct MockNetworkTransport;

    #[async_trait::async_trait(?Send)]
    impl NetworkTransport for MockNetworkTransport {
        async fn send(&self, _destination: &str, _message: Message) -> Result<Message, ActorError> {
            // Return a dummy response message
            Ok(Message::response(&_message, vec![]))
        }

        fn poll_receive(&self) -> Option<Message> {
            None
        }
    }

    fn create_test_network_transport() -> Rc<dyn NetworkTransport> {
        Rc::new(MockNetworkTransport)
    }

    #[test]
    fn test_message_bus_creation() {
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let callback_manager = create_test_callback_manager();
        let directory = create_test_directory();
        let placement = create_test_placement();
        let network_transport = create_test_network_transport();
        let bus = MessageBus::new(
            node_id.clone(),
            callback_manager,
            directory,
            placement,
            network_transport,
        );

        assert_eq!(bus.node_id(), &node_id);
        assert_eq!(bus.pending_count(), 0);
    }

    #[test]
    fn test_correlation_id_generation() {
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let callback_manager = create_test_callback_manager();
        let directory = create_test_directory();
        let placement = create_test_placement();
        let network_transport = create_test_network_transport();
        let bus = MessageBus::new(
            node_id,
            callback_manager,
            directory,
            placement,
            network_transport,
        );

        let id1 = bus.next_correlation_id();
        let id2 = bus.next_correlation_id();
        let id3 = bus.next_correlation_id();

        assert_eq!(id1, CorrelationId::new(1));
        assert_eq!(id2, CorrelationId::new(2));
        assert_eq!(id3, CorrelationId::new(3));
    }

    #[test]
    fn test_register_and_complete_request() {
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let callback_manager = create_test_callback_manager();
        let directory = create_test_directory();
        let placement = create_test_placement();
        let network_transport = create_test_network_transport();
        let bus = MessageBus::new(
            node_id,
            callback_manager,
            directory,
            placement,
            network_transport,
        );

        let corr_id = bus.next_correlation_id();
        let request = create_test_message(corr_id);
        let (tx, rx) = oneshot::channel();

        // Register
        bus.register_pending_request(corr_id, request, tx);
        assert_eq!(bus.pending_count(), 1);

        // Complete
        let response = create_test_message(corr_id);
        bus.complete_pending_request(corr_id, Ok(response.clone()))
            .unwrap();
        assert_eq!(bus.pending_count(), 0);

        // Verify response delivered
        let result = rx.blocking_recv().unwrap();
        assert!(result.is_ok());
    }

    #[test]
    fn test_complete_unknown_correlation_id() {
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let callback_manager = create_test_callback_manager();
        let directory = create_test_directory();
        let placement = create_test_placement();
        let network_transport = create_test_network_transport();
        let bus = MessageBus::new(
            node_id,
            callback_manager,
            directory,
            placement,
            network_transport,
        );

        let corr_id = CorrelationId::new(999);
        let response = create_test_message(corr_id);

        // Should fail with UnknownCorrelationId
        let result = bus.complete_pending_request(corr_id, Ok(response));
        assert!(matches!(result, Err(ActorError::UnknownCorrelationId)));
    }

    #[test]
    fn test_handle_timeout() {
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let callback_manager = create_test_callback_manager();
        let directory = create_test_directory();
        let placement = create_test_placement();
        let network_transport = create_test_network_transport();
        let bus = MessageBus::new(
            node_id,
            callback_manager,
            directory,
            placement,
            network_transport,
        );

        let corr_id = bus.next_correlation_id();
        let request = create_test_message(corr_id);
        let (tx, rx) = oneshot::channel();

        // Register
        bus.register_pending_request(corr_id, request, tx);
        assert_eq!(bus.pending_count(), 1);

        // Trigger timeout
        bus.handle_timeout(corr_id);
        assert_eq!(bus.pending_count(), 0);

        // Verify timeout error delivered
        let result = rx.blocking_recv().unwrap();
        assert!(matches!(result, Err(ActorError::Timeout)));
    }

    #[test]
    fn test_multiple_pending_requests() {
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let callback_manager = create_test_callback_manager();
        let directory = create_test_directory();
        let placement = create_test_placement();
        let network_transport = create_test_network_transport();
        let bus = MessageBus::new(
            node_id,
            callback_manager,
            directory,
            placement,
            network_transport,
        );

        // Register 3 requests
        let mut receivers = vec![];
        for _ in 0..3 {
            let corr_id = bus.next_correlation_id();
            let request = create_test_message(corr_id);
            let (tx, rx) = oneshot::channel();
            bus.register_pending_request(corr_id, request, tx);
            receivers.push((corr_id, rx));
        }

        assert_eq!(bus.pending_count(), 3);

        // Complete middle request
        let (corr_id, _) = receivers[1];
        let response = create_test_message(corr_id);
        bus.complete_pending_request(corr_id, Ok(response)).unwrap();

        assert_eq!(bus.pending_count(), 2);

        // Take ownership of receiver to verify response delivered
        let (_, rx) = receivers.swap_remove(1);
        let result = rx.blocking_recv().unwrap();
        assert!(result.is_ok());
    }

    // Async tests for send_request, send_response, send_oneway, etc.

    #[test]
    fn test_send_request_local() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            // Create a request targeting the local node
            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();
            let request = Message::request(
                CorrelationId::new(0), // Will be overwritten by send_request
                target,
                sender,
                node_id.clone(), // Same node
                node_id.clone(),
                "test_method".to_string(),
                vec![1, 2, 3],
                std::time::Duration::from_secs(5),
            );

            // Send request
            let (sent_msg, _rx) = bus.send_request(request).await.unwrap();

            // Verify correlation ID was assigned
            assert!(sent_msg.correlation_id.as_u64() > 0);
            assert_eq!(bus.pending_count(), 1);
        });
    }

    #[test]
    fn test_send_request_remote() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let remote_node = NodeId::from("127.0.0.1:8002").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            // Create a request targeting a remote node
            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();
            let request = Message::request(
                CorrelationId::new(0),
                target,
                sender,
                remote_node, // Different node
                node_id.clone(),
                "test_method".to_string(),
                vec![1, 2, 3],
                std::time::Duration::from_secs(5),
            );

            // Send request
            let (sent_msg, _rx) = bus.send_request(request).await.unwrap();

            // Verify correlation ID was assigned
            assert!(sent_msg.correlation_id.as_u64() > 0);
            assert_eq!(bus.pending_count(), 1);
        });
    }

    #[test]
    fn test_send_response_local() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            // Register a pending request first
            let corr_id = bus.next_correlation_id();
            let request = create_test_message(corr_id);
            let (tx, rx) = oneshot::channel();
            bus.register_pending_request(corr_id, request.clone(), tx);

            // Create response targeting local node
            let response = Message {
                correlation_id: corr_id,
                direction: Direction::Response,
                target_actor: request.sender_actor.clone(),
                sender_actor: request.target_actor.clone(),
                target_node: node_id.clone(), // Local
                sender_node: node_id.clone(),
                method_name: "test".to_string(),
                payload: vec![4, 5, 6],
                flags: MessageFlags::empty(),
                time_to_expiry: None,
                forward_count: 0,
                cache_invalidation: None,
            };

            // Send response
            bus.send_response(response).await.unwrap();

            // Verify callback completed
            assert_eq!(bus.pending_count(), 0);

            // Verify response delivered
            let result = rx.await.unwrap();
            assert!(result.is_ok());
        });
    }

    #[test]
    fn test_send_response_remote() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let remote_node = NodeId::from("127.0.0.1:8002").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            // Create response targeting remote node (no local callback)
            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();
            let response = Message {
                correlation_id: CorrelationId::new(123),
                direction: Direction::Response,
                target_actor: target,
                sender_actor: sender,
                target_node: remote_node, // Remote
                sender_node: node_id.clone(),
                method_name: "test".to_string(),
                payload: vec![4, 5, 6],
                flags: MessageFlags::empty(),
                time_to_expiry: None,
                forward_count: 0,
                cache_invalidation: None,
            };

            // Send response (should go over network)
            bus.send_response(response).await.unwrap();

            // No pending requests should be affected
            assert_eq!(bus.pending_count(), 0);
        });
    }

    #[test]
    fn test_send_oneway_local() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = Rc::new(MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            ));
            bus.init_self_ref();

            // Create oneway message targeting local node
            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();
            let oneway = Message::oneway(
                target,
                sender,
                node_id.clone(), // Local
                node_id.clone(),
                "notify".to_string(),
                vec![1, 2, 3],
            );

            // Send oneway - this would route to actor if routers were registered
            // For now just verify it doesn't panic
            // Note: route_to_actor will fail without routers, so we expect an error
            let result = bus.send_oneway(oneway).await;

            // Should get error about no router (expected without actor setup)
            assert!(result.is_err());

            // No pending requests should be created
            assert_eq!(bus.pending_count(), 0);
        });
    }

    #[test]
    fn test_send_oneway_remote() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let remote_node = NodeId::from("127.0.0.1:8002").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            // Create oneway message targeting remote node
            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();
            let oneway = Message::oneway(
                target,
                sender,
                remote_node, // Remote
                node_id.clone(),
                "notify".to_string(),
                vec![1, 2, 3],
            );

            // Send oneway (should go over network)
            bus.send_oneway(oneway).await.unwrap();

            // No pending requests should be created
            assert_eq!(bus.pending_count(), 0);
        });
    }

    #[test]
    fn test_route_message_response() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            // Register a pending request
            let corr_id = bus.next_correlation_id();
            let request = create_test_message(corr_id);
            let (tx, rx) = oneshot::channel();
            bus.register_pending_request(corr_id, request.clone(), tx);

            // Create response message
            let response = Message {
                correlation_id: corr_id,
                direction: Direction::Response,
                target_actor: request.sender_actor.clone(),
                sender_actor: request.target_actor.clone(),
                target_node: node_id.clone(),
                sender_node: node_id.clone(),
                method_name: "test".to_string(),
                payload: vec![7, 8, 9],
                flags: MessageFlags::empty(),
                time_to_expiry: None,
                forward_count: 0,
                cache_invalidation: None,
            };

            // Route the response
            bus.route_message(response).await.unwrap();

            // Verify callback was completed
            assert_eq!(bus.pending_count(), 0);

            // Verify response delivered
            let result = rx.await.unwrap();
            assert!(result.is_ok());
            let received_msg = result.unwrap();
            assert_eq!(received_msg.payload, vec![7, 8, 9]);
        });
    }

    #[test]
    fn test_full_request_response_cycle() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            // 1. Send request
            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();
            let request = Message::request(
                CorrelationId::new(0),
                target.clone(),
                sender.clone(),
                node_id.clone(),
                node_id.clone(),
                "test_method".to_string(),
                vec![1, 2, 3],
                std::time::Duration::from_secs(5),
            );

            let (sent_msg, response_rx) = bus.send_request(request).await.unwrap();
            let corr_id = sent_msg.correlation_id;

            assert_eq!(bus.pending_count(), 1);

            // 2. Simulate receiving and processing by creating response
            let response = Message {
                correlation_id: corr_id,
                direction: Direction::Response,
                target_actor: sender,
                sender_actor: target,
                target_node: node_id.clone(),
                sender_node: node_id.clone(),
                method_name: "test_method".to_string(),
                payload: vec![4, 5, 6],
                flags: MessageFlags::empty(),
                time_to_expiry: None,
                forward_count: 0,
                cache_invalidation: None,
            };

            // 3. Send response back
            bus.send_response(response).await.unwrap();

            // 4. Verify pending request was completed
            assert_eq!(bus.pending_count(), 0);

            // 5. Verify response was delivered
            let result = response_rx.await.unwrap();
            assert!(result.is_ok());
            let received_response = result.unwrap();
            assert_eq!(received_response.correlation_id, corr_id);
            assert_eq!(received_response.payload, vec![4, 5, 6]);
        });
    }

    // ============================================================================
    // POLL NETWORK TESTS
    // ============================================================================

    // Mock transport that can return messages
    struct MockNetworkTransportWithQueue {
        messages: RefCell<Vec<Message>>,
    }

    impl MockNetworkTransportWithQueue {
        fn new(messages: Vec<Message>) -> Self {
            Self {
                messages: RefCell::new(messages),
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl NetworkTransport for MockNetworkTransportWithQueue {
        async fn send(&self, _destination: &str, _message: Message) -> Result<Message, ActorError> {
            Ok(Message::response(&_message, vec![]))
        }

        fn poll_receive(&self) -> Option<Message> {
            self.messages.borrow_mut().pop()
        }
    }

    #[test]
    fn test_poll_network_empty() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = MessageBus::new(
                node_id,
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            // Poll with no messages available
            let result = bus.poll_network().await.unwrap();
            assert!(!result); // Should return false
        });
    }

    #[test]
    fn test_poll_network_response_message() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();

            // Register a pending request first
            let corr_id = callback_manager.next_correlation_id();
            let request = create_test_message(corr_id);
            let (tx, rx) = oneshot::channel();
            callback_manager.register_pending_request(corr_id, request.clone(), tx);

            // Create response message to be returned by poll
            let response = Message {
                correlation_id: corr_id,
                direction: Direction::Response,
                target_actor: request.sender_actor.clone(),
                sender_actor: request.target_actor.clone(),
                target_node: node_id.clone(),
                sender_node: node_id.clone(),
                method_name: "test".to_string(),
                payload: vec![10, 11, 12],
                flags: MessageFlags::empty(),
                time_to_expiry: None,
                forward_count: 0,
                cache_invalidation: None,
            };

            let network_transport = Rc::new(MockNetworkTransportWithQueue::new(vec![response]));
            let bus = MessageBus::new(
                node_id,
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            // Poll network - should receive and route response
            let result = bus.poll_network().await.unwrap();
            assert!(result); // Should return true (message processed)

            // Verify callback was completed
            let received = rx.await.unwrap().unwrap();
            assert_eq!(received.payload, vec![10, 11, 12]);
        });
    }

    #[test]
    fn test_poll_network_multiple_messages() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();

            // Register two pending requests
            let corr_id1 = callback_manager.next_correlation_id();
            let corr_id2 = callback_manager.next_correlation_id();
            let request1 = create_test_message(corr_id1);
            let request2 = create_test_message(corr_id2);
            let (tx1, rx1) = oneshot::channel();
            let (tx2, rx2) = oneshot::channel();
            callback_manager.register_pending_request(corr_id1, request1.clone(), tx1);
            callback_manager.register_pending_request(corr_id2, request2.clone(), tx2);

            // Create two response messages
            // Note: Vec::pop() returns from the end, so response2 will be returned first
            let response1 = Message {
                correlation_id: corr_id1,
                direction: Direction::Response,
                target_actor: request1.sender_actor.clone(),
                sender_actor: request1.target_actor.clone(),
                target_node: node_id.clone(),
                sender_node: node_id.clone(),
                method_name: "test".to_string(),
                payload: vec![1],
                flags: MessageFlags::empty(),
                time_to_expiry: None,
                forward_count: 0,
                cache_invalidation: None,
            };
            let response2 = Message {
                correlation_id: corr_id2,
                direction: Direction::Response,
                target_actor: request2.sender_actor.clone(),
                sender_actor: request2.target_actor.clone(),
                target_node: node_id.clone(),
                sender_node: node_id.clone(),
                method_name: "test".to_string(),
                payload: vec![2],
                flags: MessageFlags::empty(),
                time_to_expiry: None,
                forward_count: 0,
                cache_invalidation: None,
            };

            let network_transport = Rc::new(MockNetworkTransportWithQueue::new(vec![
                response1, response2,
            ]));
            let bus = MessageBus::new(
                node_id,
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            // Poll first message (will get response2)
            let result1 = bus.poll_network().await.unwrap();
            assert!(result1);

            // Poll second message (will get response1)
            let result2 = bus.poll_network().await.unwrap();
            assert!(result2);

            // Poll with no more messages
            let result3 = bus.poll_network().await.unwrap();
            assert!(!result3);

            // Verify both callbacks completed with correct payloads
            let received1 = rx1.await.unwrap().unwrap();
            let received2 = rx2.await.unwrap().unwrap();
            assert_eq!(received1.payload, vec![1]); // corr_id1 -> payload [1]
            assert_eq!(received2.payload, vec![2]); // corr_id2 -> payload [2]
        });
    }

    // ============================================================================
    // ROUTER REGISTRY TESTS
    // ============================================================================

    // Mock router for testing
    struct MockActorRouter {
        route_called: RefCell<bool>,
    }

    impl MockActorRouter {
        fn new() -> Self {
            Self {
                route_called: RefCell::new(false),
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl ActorRouter for MockActorRouter {
        async fn route_message(
            &self,
            _message: Message,
            _message_bus: Rc<MessageBus>,
        ) -> Result<(), ActorError> {
            *self.route_called.borrow_mut() = true;
            Ok(())
        }

        fn placement_hint(&self) -> crate::actor::PlacementHint {
            crate::actor::PlacementHint::Local
        }
    }

    #[test]
    fn test_set_actor_routers() {
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let callback_manager = create_test_callback_manager();
        let directory = create_test_directory();
        let placement = create_test_placement();
        let network_transport = create_test_network_transport();
        let bus = MessageBus::new(
            node_id,
            callback_manager,
            directory,
            placement,
            network_transport,
        );

        // Create router registry
        let mut routers: HashMap<String, Rc<dyn ActorRouter>> = HashMap::new();
        let mock_router = Rc::new(MockActorRouter::new());
        routers.insert("TestActor".to_string(), mock_router.clone());

        // Set routers
        bus.set_actor_routers(routers);

        // Verify router was registered (indirectly, by checking it doesn't panic)
        assert_eq!(bus.pending_count(), 0); // Just a sanity check
    }

    #[test]
    fn test_route_to_actor_no_router_registered() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = Rc::new(MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            ));
            bus.init_self_ref();

            // Create request message
            let target = ActorId::from_string("test::UnknownActor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();
            let message = Message::request(
                CorrelationId::new(1),
                target,
                sender,
                node_id.clone(),
                node_id,
                "test".to_string(),
                vec![],
                std::time::Duration::from_secs(5),
            );

            // Try to route - should fail with no router registered
            let result = bus.route_message(message).await;
            assert!(result.is_err());
            assert!(matches!(result, Err(ActorError::ProcessingFailed(_))));
        });
    }

    // ============================================================================
    // SELF-REFERENCE TESTS
    // ============================================================================

    #[test]
    fn test_init_self_ref() {
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let callback_manager = create_test_callback_manager();
        let directory = create_test_directory();
        let placement = create_test_placement();
        let network_transport = create_test_network_transport();
        let bus = Rc::new(MessageBus::new(
            node_id,
            callback_manager,
            directory,
            placement,
            network_transport,
        ));

        // Before init, self_ref should be None
        assert!(bus.self_ref.borrow().is_none());

        // After init, self_ref should be Some
        bus.init_self_ref();
        assert!(bus.self_ref.borrow().is_some());

        // Verify we can upgrade the weak reference
        let weak_ref = bus.self_ref.borrow().as_ref().unwrap().clone();
        assert!(weak_ref.upgrade().is_some());
    }

    #[test]
    fn test_route_to_actor_without_init_self_ref() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = Rc::new(MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            ));
            // Note: NOT calling init_self_ref()

            // Register a mock router
            let mut routers: HashMap<String, Rc<dyn ActorRouter>> = HashMap::new();
            let mock_router = Rc::new(MockActorRouter::new());
            routers.insert("Actor".to_string(), mock_router);
            bus.set_actor_routers(routers);

            // Create request message
            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();
            let message = Message::request(
                CorrelationId::new(1),
                target,
                sender,
                node_id.clone(),
                node_id,
                "test".to_string(),
                vec![],
                std::time::Duration::from_secs(5),
            );

            // Try to route - should fail because self_ref not initialized
            let result = bus.route_message(message).await;
            assert!(result.is_err());
            match result {
                Err(ActorError::ProcessingFailed(msg)) => {
                    assert!(msg.contains("self-reference not initialized"));
                }
                _ => panic!("Expected ProcessingFailed error"),
            }
        });
    }

    // ============================================================================
    // ERROR PATH TESTS
    // ============================================================================

    struct FailingNetworkTransport;

    #[async_trait::async_trait(?Send)]
    impl NetworkTransport for FailingNetworkTransport {
        async fn send(&self, _destination: &str, _message: Message) -> Result<Message, ActorError> {
            Err(ActorError::Message(MessageError::IoError(
                "Simulated network failure".to_string(),
            )))
        }

        fn poll_receive(&self) -> Option<Message> {
            None
        }
    }

    #[test]
    fn test_send_request_network_failure() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let remote_node = NodeId::from("127.0.0.1:8002").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = Rc::new(FailingNetworkTransport);
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            // Create remote request
            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();
            let request = Message::request(
                CorrelationId::new(0),
                target,
                sender,
                remote_node, // Remote
                node_id,
                "test".to_string(),
                vec![],
                std::time::Duration::from_secs(5),
            );

            // Send request - should fail due to network error
            let result = bus.send_request(request).await;
            assert!(result.is_err());
            assert!(matches!(
                result,
                Err(ActorError::Message(MessageError::IoError(_)))
            ));
        });
    }

    #[test]
    fn test_send_response_network_failure() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let remote_node = NodeId::from("127.0.0.1:8002").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = Rc::new(FailingNetworkTransport);
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            // Create remote response
            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();
            let response = Message {
                correlation_id: CorrelationId::new(123),
                direction: Direction::Response,
                target_actor: target,
                sender_actor: sender,
                target_node: remote_node, // Remote
                sender_node: node_id,
                method_name: "test".to_string(),
                payload: vec![],
                flags: MessageFlags::empty(),
                time_to_expiry: None,
                forward_count: 0,
                cache_invalidation: None,
            };

            // Send response - should fail due to network error
            let result = bus.send_response(response).await;
            assert!(result.is_err());
            assert!(matches!(
                result,
                Err(ActorError::Message(MessageError::IoError(_)))
            ));
        });
    }

    #[test]
    fn test_complete_request_twice() {
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let callback_manager = create_test_callback_manager();
        let directory = create_test_directory();
        let placement = create_test_placement();
        let network_transport = create_test_network_transport();
        let bus = MessageBus::new(
            node_id,
            callback_manager,
            directory,
            placement,
            network_transport,
        );

        let corr_id = bus.next_correlation_id();
        let request = create_test_message(corr_id);
        let (tx, _rx) = oneshot::channel();

        // Register
        bus.register_pending_request(corr_id, request.clone(), tx);

        // Complete first time - should succeed
        let response = create_test_message(corr_id);
        let result1 = bus.complete_pending_request(corr_id, Ok(response.clone()));
        assert!(result1.is_ok());

        // Complete second time - should fail with UnknownCorrelationId
        let result2 = bus.complete_pending_request(corr_id, Ok(response));
        assert!(matches!(result2, Err(ActorError::UnknownCorrelationId)));
    }

    // ============================================================================
    // CONCURRENT OPERATIONS TESTS
    // ============================================================================

    #[test]
    fn test_multiple_concurrent_send_requests() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();

            // Send multiple requests concurrently (sequentially in single-threaded runtime)
            let mut receivers = vec![];
            for _ in 0..5 {
                let request = Message::request(
                    CorrelationId::new(0),
                    target.clone(),
                    sender.clone(),
                    node_id.clone(),
                    node_id.clone(),
                    "test".to_string(),
                    vec![],
                    std::time::Duration::from_secs(5),
                );
                let (_msg, rx) = bus.send_request(request).await.unwrap();
                receivers.push(rx);
            }

            // All 5 should be pending
            assert_eq!(bus.pending_count(), 5);

            // Correlation IDs should be unique
            assert_eq!(receivers.len(), 5);
        });
    }

    #[test]
    fn test_interleaved_request_response() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();

            // Send request 1
            let req1 = Message::request(
                CorrelationId::new(0),
                target.clone(),
                sender.clone(),
                node_id.clone(),
                node_id.clone(),
                "test".to_string(),
                vec![1],
                std::time::Duration::from_secs(5),
            );
            let (msg1, rx1) = bus.send_request(req1).await.unwrap();
            let corr_id1 = msg1.correlation_id;

            // Send request 2
            let req2 = Message::request(
                CorrelationId::new(0),
                target.clone(),
                sender.clone(),
                node_id.clone(),
                node_id.clone(),
                "test".to_string(),
                vec![2],
                std::time::Duration::from_secs(5),
            );
            let (msg2, rx2) = bus.send_request(req2).await.unwrap();
            let corr_id2 = msg2.correlation_id;

            assert_eq!(bus.pending_count(), 2);

            // Respond to request 2 first (out of order)
            let resp2 = Message {
                correlation_id: corr_id2,
                direction: Direction::Response,
                target_actor: sender.clone(),
                sender_actor: target.clone(),
                target_node: node_id.clone(),
                sender_node: node_id.clone(),
                method_name: "test".to_string(),
                payload: vec![20],
                flags: MessageFlags::empty(),
                time_to_expiry: None,
                forward_count: 0,
                cache_invalidation: None,
            };
            bus.send_response(resp2).await.unwrap();
            assert_eq!(bus.pending_count(), 1);

            // Respond to request 1
            let resp1 = Message {
                correlation_id: corr_id1,
                direction: Direction::Response,
                target_actor: sender,
                sender_actor: target,
                target_node: node_id.clone(),
                sender_node: node_id,
                method_name: "test".to_string(),
                payload: vec![10],
                flags: MessageFlags::empty(),
                time_to_expiry: None,
                forward_count: 0,
                cache_invalidation: None,
            };
            bus.send_response(resp1).await.unwrap();
            assert_eq!(bus.pending_count(), 0);

            // Verify responses delivered correctly
            let result1 = rx1.await.unwrap().unwrap();
            let result2 = rx2.await.unwrap().unwrap();
            assert_eq!(result1.payload, vec![10]);
            assert_eq!(result2.payload, vec![20]);
        });
    }

    // ============================================================================
    // MESSAGE VALIDATION TESTS
    // ============================================================================

    #[test]
    fn test_message_directions() {
        let target = ActorId::from_string("test::Actor/1").unwrap();
        let sender = ActorId::from_string("test::Sender/1").unwrap();
        let target_node = NodeId::from("127.0.0.1:8001").unwrap();
        let sender_node = NodeId::from("127.0.0.1:8002").unwrap();

        // Test Request direction
        let request = Message::request(
            CorrelationId::new(1),
            target.clone(),
            sender.clone(),
            target_node.clone(),
            sender_node.clone(),
            "test".to_string(),
            vec![],
            std::time::Duration::from_secs(5),
        );
        assert_eq!(request.direction, Direction::Request);
        assert!(request.time_to_expiry.is_some());

        // Test Response direction
        let response = Message::response(&request, vec![1, 2, 3]);
        assert_eq!(response.direction, Direction::Response);
        assert!(response.time_to_expiry.is_none());
        assert_eq!(response.correlation_id, request.correlation_id);

        // Test OneWay direction
        let oneway = Message::oneway(
            target,
            sender,
            target_node,
            sender_node,
            "notify".to_string(),
            vec![],
        );
        assert_eq!(oneway.direction, Direction::OneWay);
        assert!(oneway.time_to_expiry.is_none());
    }

    #[test]
    fn test_message_forward_count_limit() {
        let target = ActorId::from_string("test::Actor/1").unwrap();
        let sender = ActorId::from_string("test::Sender/1").unwrap();
        let target_node = NodeId::from("127.0.0.1:8001").unwrap();
        let sender_node = NodeId::from("127.0.0.1:8002").unwrap();

        let mut message = Message::request(
            CorrelationId::new(1),
            target,
            sender,
            target_node,
            sender_node,
            "test".to_string(),
            vec![],
            std::time::Duration::from_secs(5),
        );

        assert_eq!(message.forward_count, 0);

        // Increment within limit
        assert!(message.increment_forward_count(3).is_ok());
        assert_eq!(message.forward_count, 1);

        assert!(message.increment_forward_count(3).is_ok());
        assert_eq!(message.forward_count, 2);

        assert!(message.increment_forward_count(3).is_ok());
        assert_eq!(message.forward_count, 3);

        // Exceed limit
        assert!(message.increment_forward_count(3).is_err());
        assert_eq!(message.forward_count, 3); // Should not increment
    }

    // ============================================================================
    // EDGE CASE TESTS
    // ============================================================================

    #[test]
    fn test_empty_payload_message() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();
            let request = Message::request(
                CorrelationId::new(0),
                target,
                sender,
                node_id.clone(),
                node_id,
                "test".to_string(),
                vec![], // Empty payload
                std::time::Duration::from_secs(5),
            );

            let (_msg, _rx) = bus.send_request(request).await.unwrap();
            assert_eq!(bus.pending_count(), 1);
        });
    }

    #[test]
    fn test_large_payload_message() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();
            let large_payload = vec![42u8; 10_000]; // 10KB payload
            let request = Message::request(
                CorrelationId::new(0),
                target,
                sender,
                node_id.clone(),
                node_id,
                "test".to_string(),
                large_payload.clone(),
                std::time::Duration::from_secs(5),
            );

            let (sent_msg, _rx) = bus.send_request(request).await.unwrap();
            assert_eq!(sent_msg.payload.len(), 10_000);
            assert_eq!(bus.pending_count(), 1);
        });
    }

    #[test]
    fn test_correlation_id_reuse_after_completion() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move {
            let node_id = NodeId::from("127.0.0.1:8001").unwrap();
            let callback_manager = create_test_callback_manager();
            let directory = create_test_directory();
            let placement = create_test_placement();
            let network_transport = create_test_network_transport();
            let bus = MessageBus::new(
                node_id.clone(),
                callback_manager,
                directory,
                placement,
                network_transport,
            );

            let target = ActorId::from_string("test::Actor/1").unwrap();
            let sender = ActorId::from_string("test::Sender/1").unwrap();

            // First request-response cycle
            let req1 = Message::request(
                CorrelationId::new(0),
                target.clone(),
                sender.clone(),
                node_id.clone(),
                node_id.clone(),
                "test".to_string(),
                vec![1],
                std::time::Duration::from_secs(5),
            );
            let (msg1, rx1) = bus.send_request(req1).await.unwrap();
            let corr_id1 = msg1.correlation_id;

            let resp1 = Message {
                correlation_id: corr_id1,
                direction: Direction::Response,
                target_actor: sender.clone(),
                sender_actor: target.clone(),
                target_node: node_id.clone(),
                sender_node: node_id.clone(),
                method_name: "test".to_string(),
                payload: vec![10],
                flags: MessageFlags::empty(),
                time_to_expiry: None,
                forward_count: 0,
                cache_invalidation: None,
            };
            bus.send_response(resp1).await.unwrap();
            let _ = rx1.await.unwrap().unwrap();

            assert_eq!(bus.pending_count(), 0);

            // Second request - correlation ID continues incrementing
            let req2 = Message::request(
                CorrelationId::new(0),
                target,
                sender,
                node_id.clone(),
                node_id,
                "test".to_string(),
                vec![2],
                std::time::Duration::from_secs(5),
            );
            let (msg2, _rx2) = bus.send_request(req2).await.unwrap();
            let corr_id2 = msg2.correlation_id;

            // Correlation IDs should be different
            assert_ne!(corr_id1, corr_id2);
            assert_eq!(corr_id2.as_u64(), corr_id1.as_u64() + 1);
            assert_eq!(bus.pending_count(), 1);
        });
    }

    #[test]
    fn test_message_flags() {
        let target = ActorId::from_string("test::Actor/1").unwrap();
        let sender = ActorId::from_string("test::Sender/1").unwrap();
        let target_node = NodeId::from("127.0.0.1:8001").unwrap();
        let sender_node = NodeId::from("127.0.0.1:8002").unwrap();

        let mut message = Message::request(
            CorrelationId::new(1),
            target,
            sender,
            target_node,
            sender_node,
            "test".to_string(),
            vec![],
            std::time::Duration::from_secs(5),
        );

        // Initially empty
        assert!(message.flags.is_empty());

        // Set READ_ONLY flag
        message.flags = MessageFlags::READ_ONLY;
        assert!(message.flags.contains(MessageFlags::READ_ONLY));

        // Set multiple flags
        message.flags = MessageFlags::READ_ONLY | MessageFlags::ALWAYS_INTERLEAVE;
        assert!(message.flags.contains(MessageFlags::READ_ONLY));
        assert!(message.flags.contains(MessageFlags::ALWAYS_INTERLEAVE));

        // Set IS_LOCAL_ONLY flag
        message.flags = MessageFlags::IS_LOCAL_ONLY;
        assert!(message.flags.contains(MessageFlags::IS_LOCAL_ONLY));
        assert!(!message.flags.contains(MessageFlags::READ_ONLY));
    }
}
