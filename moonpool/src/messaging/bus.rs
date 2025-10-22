//! Message bus for routing messages to local actors.
//!
//! This module provides the `MessageBus` which routes incoming messages to
//! the appropriate local actors via the ActorCatalog.
//!
//! # Architecture (Simplified for Phase 3)
//!
//! This is a simplified MessageBus for local message routing. Full network
//! integration with PeerTransport will be added in later phases.
//!
//! ```text
//! ┌────────────────────────────────────┐
//! │ MessageBus                         │
//! │                                    │
//! │  ┌──────────────────────────────┐  │
//! │  │ node_id: NodeId              │  │
//! │  └──────────────────────────────┘  │
//! │                                    │
//! │  ┌──────────────────────────────┐  │
//! │  │ next_correlation_id: Cell    │  │
//! │  └──────────────────────────────┘  │
//! │                                    │
//! │  ┌──────────────────────────────┐  │
//! │  │ pending_requests: RefCell    │  │
//! │  └──────────────────────────────┘  │
//! └────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use moonpool::messaging::MessageBus;
//!
//! // Create message bus
//! let node_id = NodeId::from("127.0.0.1:8001")?;
//! let bus = MessageBus::new(node_id);
//!
//! // Generate correlation ID
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
use crate::error::ActorError;
use crate::messaging::{CallbackData, Message};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use tokio::sync::oneshot;

/// Message bus for routing messages to actors.
///
/// `MessageBus` handles correlation ID generation, pending request tracking,
/// and message routing. This is a simplified version for Phase 3 that focuses
/// on local routing. Full network integration will be added later.
///
/// # Single-Threaded Design
///
/// Uses `Cell` and `RefCell` for interior mutability (no Send/Sync required).
/// Compatible with tokio's `current_thread` runtime.
///
/// # Example
///
/// ```rust,ignore
/// let bus = MessageBus::new(node_id);
///
/// // Generate correlation ID for request
/// let corr_id = bus.next_correlation_id();
///
/// // Register callback
/// let (tx, rx) = oneshot::channel();
/// bus.register_pending_request(corr_id, request, tx);
///
/// // ... send message over network ...
///
/// // When response arrives:
/// bus.complete_pending_request(corr_id, Ok(response));
/// ```
pub struct MessageBus {
    /// This node's identifier.
    node_id: NodeId,

    /// Next correlation ID (monotonically increasing).
    ///
    /// Uses Cell for single-threaded increment (no atomics needed).
    next_correlation_id: Cell<u64>,

    /// Pending requests awaiting responses.
    ///
    /// Maps correlation_id → CallbackData.
    /// Uses RefCell for interior mutability.
    pending_requests: RefCell<HashMap<CorrelationId, CallbackData>>,
}

impl MessageBus {
    /// Create a new MessageBus for this node.
    ///
    /// # Parameters
    ///
    /// - `node_id`: This node's identifier
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let node_id = NodeId::from("127.0.0.1:8001")?;
    /// let bus = MessageBus::new(node_id);
    /// ```
    pub fn new(node_id: NodeId) -> Self {
        Self {
            node_id,
            next_correlation_id: Cell::new(1),
            pending_requests: RefCell::new(HashMap::new()),
        }
    }

    /// Get this node's ID.
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    /// Generate the next correlation ID.
    ///
    /// Returns monotonically increasing IDs starting from 1.
    /// Unique per node (not globally unique).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let id1 = bus.next_correlation_id();
    /// let id2 = bus.next_correlation_id();
    /// assert!(id2.value() > id1.value());
    /// ```
    pub fn next_correlation_id(&self) -> CorrelationId {
        let id = self.next_correlation_id.get();
        self.next_correlation_id.set(id + 1);
        CorrelationId::new(id)
    }

    /// Register a pending request awaiting response.
    ///
    /// Creates a CallbackData to track the request and stores it in the
    /// pending_requests map for later correlation.
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
        let callback = CallbackData::new(message, sender);
        self.pending_requests
            .borrow_mut()
            .insert(correlation_id, callback);
    }

    /// Complete a pending request with a response.
    ///
    /// Looks up the CallbackData by correlation ID and completes it with
    /// the provided result. Removes the callback from pending_requests.
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
        let callback = self
            .pending_requests
            .borrow_mut()
            .remove(&correlation_id)
            .ok_or(ActorError::UnknownCorrelationId)?;

        callback.complete(result);
        Ok(())
    }

    /// Handle a timeout for a pending request.
    ///
    /// Removes the callback from pending_requests and completes it with a timeout error.
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
        if let Some(callback) = self.pending_requests.borrow_mut().remove(&correlation_id) {
            callback.on_timeout();
        }
    }

    /// Get the number of pending requests.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let count = bus.pending_count();
    /// println!("Pending requests: {}", count);
    /// ```
    pub fn pending_count(&self) -> usize {
        self.pending_requests.borrow().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::{ActorId, CorrelationId};
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

    #[test]
    fn test_message_bus_creation() {
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let bus = MessageBus::new(node_id.clone());

        assert_eq!(bus.node_id(), &node_id);
        assert_eq!(bus.pending_count(), 0);
    }

    #[test]
    fn test_correlation_id_generation() {
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let bus = MessageBus::new(node_id);

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
        let bus = MessageBus::new(node_id);

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
        let bus = MessageBus::new(node_id);

        let corr_id = CorrelationId::new(999);
        let response = create_test_message(corr_id);

        // Should fail with UnknownCorrelationId
        let result = bus.complete_pending_request(corr_id, Ok(response));
        assert!(matches!(result, Err(ActorError::UnknownCorrelationId)));
    }

    #[test]
    fn test_handle_timeout() {
        let node_id = NodeId::from("127.0.0.1:8001").unwrap();
        let bus = MessageBus::new(node_id);

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
        let bus = MessageBus::new(node_id);

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
}
