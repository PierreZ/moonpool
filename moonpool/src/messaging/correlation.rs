//! Callback data for request-response correlation.
//!
//! This module provides the `CallbackData` struct which manages the lifecycle
//! of a pending request waiting for a response. It handles timeouts, completion
//! tracking, and error cases.
//!
//! # Architecture
//!
//! ```text
//! Request Flow:
//!   1. Create CallbackData with oneshot::Sender
//!   2. Store in MessageBus pending_requests map
//!   3. Spawn timeout task
//!   4. Send request over network
//!   5. Wait on oneshot::Receiver
//!
//! Response Flow (Success):
//!   6. Response arrives
//!   7. Lookup correlation_id in pending_requests
//!   8. Call complete() with response
//!   9. oneshot::Sender fires
//!  10. Waiting task receives response
//!
//! Response Flow (Timeout):
//!   6. Timeout elapses
//!   7. Call on_timeout()
//!   8. Complete with ActorError::Timeout
//!   9. Waiting task receives error
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use moonpool::messaging::CallbackData;
//! use tokio::sync::oneshot;
//!
//! // Create callback
//! let (tx, rx) = oneshot::channel();
//! let callback = CallbackData::new(message, tx);
//!
//! // Store in pending_requests
//! pending_requests.insert(correlation_id, callback);
//!
//! // Spawn timeout task
//! task_provider.spawn_task(async move {
//!     time_provider.sleep(timeout).await;
//!     if let Some(cb) = pending_requests.remove(&correlation_id) {
//!         cb.on_timeout();
//!     }
//! });
//!
//! // Wait for response
//! match rx.await {
//!     Ok(response) => // Handle response
//!     Err(_) => // Channel closed (timeout or error)
//! }
//! ```

use crate::error::ActorError;
use crate::messaging::Message;
use std::cell::Cell;
use std::time::Instant;
use tokio::sync::oneshot;

/// Callback data for tracking a pending request awaiting response.
///
/// `CallbackData` holds the state for a single request-response correlation.
/// It tracks completion status, elapsed time, and provides the oneshot channel
/// for delivering the response back to the caller.
///
/// # Lifecycle
///
/// 1. **Created**: When request is sent (before network transmission)
/// 2. **Pending**: Stored in MessageBus pending_requests map
/// 3. **Completed**: Either by response arrival, timeout, or error
/// 4. **Removed**: From pending_requests after completion
///
/// # Thread Safety
///
/// Uses `Cell<bool>` for completion flag (single-threaded execution).
/// The oneshot::Sender is moved into the struct and consumed on completion.
///
/// # Example
///
/// ```rust,ignore
/// let (tx, rx) = oneshot::channel();
/// let callback = CallbackData::new(request_message, tx);
///
/// // Later, when response arrives:
/// callback.complete(Ok(response_message));
///
/// // Or when timeout occurs:
/// callback.on_timeout();
/// ```
pub struct CallbackData {
    /// The original request message.
    ///
    /// Stored for debugging and error reporting. Contains correlation_id,
    /// target actor, and other metadata.
    message: Message,

    /// Oneshot sender for delivering the response.
    ///
    /// Wrapped in Option so it can be consumed on completion.
    /// None after completion.
    sender: Cell<Option<oneshot::Sender<Result<Message, ActorError>>>>,

    /// Timestamp when request was sent.
    ///
    /// Used for calculating elapsed time and detecting timeouts.
    start_time: Instant,

    /// Whether this callback has been completed.
    ///
    /// Set to true by complete(), on_timeout(), or on_error().
    /// Prevents double completion.
    completed: Cell<bool>,
}

impl CallbackData {
    /// Create a new CallbackData for a pending request.
    ///
    /// # Parameters
    ///
    /// - `message`: The request message being sent
    /// - `sender`: Oneshot channel sender for delivering the response
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (tx, rx) = oneshot::channel();
    /// let callback = CallbackData::new(request, tx);
    ///
    /// // Store in pending map
    /// pending_requests.insert(correlation_id, callback);
    ///
    /// // Wait for response
    /// let response = rx.await?;
    /// ```
    pub fn new(message: Message, sender: oneshot::Sender<Result<Message, ActorError>>) -> Self {
        Self {
            message,
            sender: Cell::new(Some(sender)),
            start_time: Instant::now(),
            completed: Cell::new(false),
        }
    }

    /// Get the original request message.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let msg = callback.message();
    /// println!("Request to: {}", msg.target);
    /// ```
    pub fn message(&self) -> &Message {
        &self.message
    }

    /// Get elapsed time since request was sent.
    ///
    /// Used for timeout detection and metrics.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let elapsed = callback.elapsed();
    /// if elapsed > timeout_duration {
    ///     callback.on_timeout();
    /// }
    /// ```
    pub fn elapsed(&self) -> std::time::Duration {
        self.start_time.elapsed()
    }

    /// Check if this callback has been completed.
    ///
    /// Returns true after complete(), on_timeout(), or on_error() has been called.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if !callback.is_completed() {
    ///     callback.on_timeout();
    /// }
    /// ```
    pub fn is_completed(&self) -> bool {
        self.completed.get()
    }

    /// Complete the callback with a response.
    ///
    /// Sends the response through the oneshot channel to the waiting caller.
    /// This method is idempotent - calling it multiple times has no effect
    /// after the first successful completion.
    ///
    /// # Parameters
    ///
    /// - `result`: The response message or error
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // On response arrival:
    /// callback.complete(Ok(response_message));
    ///
    /// // On error:
    /// callback.complete(Err(ActorError::NodeUnavailable(node_id)));
    /// ```
    pub fn complete(&self, result: Result<Message, ActorError>) {
        // Check if already completed (idempotent)
        if self.completed.get() {
            return;
        }

        // Mark as completed
        self.completed.set(true);

        // Send result through oneshot channel
        if let Some(sender) = self.sender.take() {
            // Send and log any errors (receiver might have been dropped)
            match sender.send(result) {
                Ok(()) => {
                    tracing::debug!("CallbackData::complete: oneshot send succeeded");
                }
                Err(_) => {
                    tracing::warn!(
                        "CallbackData::complete: oneshot send failed - receiver dropped"
                    );
                }
            }
        } else {
            tracing::warn!("CallbackData::complete: sender already taken (double completion?)");
        }
    }

    /// Handle timeout expiration.
    ///
    /// Called when the request timeout elapses before a response arrives.
    /// Completes the callback with ActorError::Timeout.
    ///
    /// This method is idempotent - if the callback was already completed
    /// (e.g., response arrived just before timeout), this has no effect.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // In timeout task:
    /// time_provider.sleep(timeout_duration).await;
    /// if let Some(callback) = pending_requests.remove(&correlation_id) {
    ///     callback.on_timeout();
    /// }
    /// ```
    pub fn on_timeout(&self) {
        self.complete(Err(ActorError::Timeout));
    }

    /// Handle error condition.
    ///
    /// Called when an error occurs before a response can be received
    /// (e.g., network failure, actor not found, serialization error).
    ///
    /// # Parameters
    ///
    /// - `error`: The error that occurred
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // On network error:
    /// callback.on_error(ActorError::NodeUnavailable(target_node));
    ///
    /// // On actor not found:
    /// callback.on_error(ActorError::NotFound(format!("Actor: {}", actor_id)));
    /// ```
    pub fn on_error(&self, error: ActorError) {
        self.complete(Err(error));
    }
}

/// Response type sent through the oneshot channel.
///
/// Wraps the response message or error for delivery to the waiting caller.
pub type CallbackResponse = Result<Message, ActorError>;

/// Factory for creating correlation IDs.
///
/// Provides monotonically increasing correlation IDs for request tracking.
/// Uses Cell<u64> for single-threaded increment.
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::messaging::CorrelationIdFactory;
///
/// let factory = CorrelationIdFactory::new();
/// let id1 = factory.next();
/// let id2 = factory.next();
/// assert!(id2 > id1);
/// ```
#[derive(Debug)]
pub struct CorrelationIdFactory {
    next_id: Cell<u64>,
}

impl CorrelationIdFactory {
    /// Create a new CorrelationIdFactory starting from 1.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let factory = CorrelationIdFactory::new();
    /// ```
    pub fn new() -> Self {
        Self {
            next_id: Cell::new(1),
        }
    }

    /// Generate the next correlation ID.
    ///
    /// Returns monotonically increasing IDs starting from 1.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let id = factory.next();
    /// println!("Correlation ID: {}", id);
    /// ```
    pub fn next(&self) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        id
    }
}

impl Default for CorrelationIdFactory {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared configuration for callback management.
///
/// Holds common settings like default timeout duration that apply to all
/// callbacks in a MessageBus.
#[derive(Debug, Clone)]
pub struct CallbackConfig {
    /// Default timeout for requests (if not overridden).
    pub default_timeout: std::time::Duration,
}

impl Default for CallbackConfig {
    fn default() -> Self {
        Self {
            // Default 30 second timeout (Orleans default)
            default_timeout: std::time::Duration::from_secs(30),
        }
    }
}

impl CallbackConfig {
    /// Create a new CallbackConfig with custom timeout.
    ///
    /// # Parameters
    ///
    /// - `timeout`: Default timeout duration for requests
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use std::time::Duration;
    /// use moonpool::messaging::CallbackConfig;
    ///
    /// let config = CallbackConfig::with_timeout(Duration::from_secs(60));
    /// ```
    pub fn with_timeout(timeout: std::time::Duration) -> Self {
        Self {
            default_timeout: timeout,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::{ActorId, CorrelationId, NodeId};
    use crate::messaging::{Direction, Message, MessageFlags};

    fn create_test_message() -> Message {
        let target = ActorId::from_string("test::Actor/1").unwrap();
        let sender = ActorId::from_string("test::Sender/1").unwrap();
        let target_node = NodeId::from("127.0.0.1:8001").unwrap();
        let sender_node = NodeId::from("127.0.0.1:8002").unwrap();

        Message {
            correlation_id: CorrelationId::new(1),
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
    fn test_callback_data_complete_success() {
        let (tx, rx) = oneshot::channel();
        let request = create_test_message();
        let callback = CallbackData::new(request, tx);

        assert!(!callback.is_completed());

        // Complete with response
        let response = create_test_message();
        callback.complete(Ok(response.clone()));

        assert!(callback.is_completed());

        // Verify response delivered
        let result = rx.blocking_recv().unwrap();
        assert!(result.is_ok());
    }

    #[test]
    fn test_callback_data_timeout() {
        let (tx, rx) = oneshot::channel();
        let request = create_test_message();
        let callback = CallbackData::new(request, tx);

        // Trigger timeout
        callback.on_timeout();

        assert!(callback.is_completed());

        // Verify timeout error delivered
        let result = rx.blocking_recv().unwrap();
        assert!(matches!(result, Err(ActorError::Timeout)));
    }

    #[test]
    fn test_callback_data_error() {
        let (tx, rx) = oneshot::channel();
        let request = create_test_message();
        let callback = CallbackData::new(request, tx);

        // Trigger error
        let node_id = NodeId::from("127.0.0.1:9999").unwrap();
        callback.on_error(ActorError::NodeUnavailable(node_id));

        assert!(callback.is_completed());

        // Verify error delivered
        let result = rx.blocking_recv().unwrap();
        assert!(matches!(result, Err(ActorError::NodeUnavailable(_))));
    }

    #[test]
    fn test_callback_data_idempotent_completion() {
        let (tx, rx) = oneshot::channel();
        let request = create_test_message();
        let callback = CallbackData::new(request, tx);

        // Complete multiple times
        let response1 = create_test_message();
        let response2 = create_test_message();

        callback.complete(Ok(response1.clone()));
        callback.complete(Ok(response2)); // Should have no effect
        callback.on_timeout(); // Should have no effect

        // First completion wins
        let result = rx.blocking_recv().unwrap();
        assert!(result.is_ok());
    }

    #[test]
    fn test_callback_data_elapsed_time() {
        let (tx, _rx) = oneshot::channel();
        let request = create_test_message();
        let callback = CallbackData::new(request, tx);

        // Small delay
        std::thread::sleep(std::time::Duration::from_millis(10));

        let elapsed = callback.elapsed();
        assert!(elapsed >= std::time::Duration::from_millis(10));
        assert!(elapsed < std::time::Duration::from_millis(100));
    }

    #[test]
    fn test_correlation_id_factory() {
        let factory = CorrelationIdFactory::new();

        let id1 = factory.next();
        let id2 = factory.next();
        let id3 = factory.next();

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_callback_config_default() {
        let config = CallbackConfig::default();
        assert_eq!(config.default_timeout, std::time::Duration::from_secs(30));
    }

    #[test]
    fn test_callback_config_custom_timeout() {
        let timeout = std::time::Duration::from_secs(60);
        let config = CallbackConfig::with_timeout(timeout);
        assert_eq!(config.default_timeout, timeout);
    }
}
