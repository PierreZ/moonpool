//! NetNotifiedQueue: Typed message queue with async notification.
//!
//! Deserializes incoming bytes into typed messages and provides
//! async waiting for new messages via Waker-based notification.
//!
//! # FDB Reference
//! Based on PromiseStream internal queue pattern from fdbrpc.h

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use serde::de::DeserializeOwned;

use moonpool_foundation::{Endpoint, NetworkAddress, UID, sometimes_assert};

use super::endpoint_map::MessageReceiver;

/// Type-safe message queue with async notification.
///
/// Receives raw bytes, deserializes them to type `T`, and queues them.
/// Consumers can async wait for messages using `recv()` or poll with `try_recv()`.
///
/// # Design
///
/// - Uses `RefCell` for single-threaded runtime (no Mutex overhead)
/// - Waker-based notification wakes all waiting consumers
/// - Deserializes on receive (producer side) to fail fast on bad messages
///
/// # Type Safety
///
/// The type `T` is baked in at compile time. Only messages that deserialize
/// to `T` will be accepted. Invalid messages log an error and are dropped.
pub struct NetNotifiedQueue<T> {
    /// Internal state wrapped in RefCell for interior mutability.
    inner: RefCell<NetNotifiedQueueInner<T>>,

    /// Endpoint associated with this queue.
    endpoint: Endpoint,
}

/// Internal state for the queue.
struct NetNotifiedQueueInner<T> {
    /// Message queue (FIFO).
    queue: VecDeque<T>,

    /// Wakers waiting for messages.
    wakers: Vec<Waker>,

    /// Whether the queue has been closed (no more messages expected).
    closed: bool,

    /// Statistics for debugging.
    messages_received: u64,
    messages_dropped: u64,
}

impl<T> Default for NetNotifiedQueueInner<T> {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            wakers: Vec::new(),
            closed: false,
            messages_received: 0,
            messages_dropped: 0,
        }
    }
}

impl<T> NetNotifiedQueue<T> {
    /// Create a new queue with the given endpoint.
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            inner: RefCell::new(NetNotifiedQueueInner::default()),
            endpoint,
        }
    }

    /// Create a new queue with a dynamically allocated endpoint.
    ///
    /// Uses the provided address with a new random UID.
    pub fn with_address(address: NetworkAddress) -> Self {
        // In real usage, UID should be generated via RandomProvider for determinism.
        // For now, use a simple sequential ID.
        let token = UID::new(0, rand_simple_id());
        Self::new(Endpoint::new(address, token))
    }

    /// Get the endpoint for this queue.
    ///
    /// Senders use this endpoint to address messages to this queue.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Try to receive a message without blocking.
    ///
    /// Returns `None` if no message is available.
    pub fn try_recv(&self) -> Option<T> {
        self.inner.borrow_mut().queue.pop_front()
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().queue.is_empty()
    }

    /// Get the number of messages currently in the queue.
    pub fn len(&self) -> usize {
        self.inner.borrow().queue.len()
    }

    /// Get the total number of messages received.
    pub fn messages_received(&self) -> u64 {
        self.inner.borrow().messages_received
    }

    /// Get the number of messages dropped due to deserialization errors.
    pub fn messages_dropped(&self) -> u64 {
        self.inner.borrow().messages_dropped
    }

    /// Mark the queue as closed.
    ///
    /// After closing, `recv()` will return `None` when the queue is empty
    /// instead of waiting for more messages.
    pub fn close(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.closed = true;
        // Wake all waiters to let them see the close
        for waker in inner.wakers.drain(..) {
            waker.wake();
        }
    }

    /// Check if the queue is closed.
    pub fn is_closed(&self) -> bool {
        self.inner.borrow().closed
    }

    /// Push a pre-deserialized message directly (for testing).
    #[cfg(test)]
    fn push(&self, message: T) {
        let mut inner = self.inner.borrow_mut();
        inner.queue.push_back(message);
        inner.messages_received += 1;
        // Wake all waiters
        for waker in inner.wakers.drain(..) {
            waker.wake();
        }
    }
}

impl<T: DeserializeOwned> NetNotifiedQueue<T> {
    /// Async receive - waits for a message.
    ///
    /// Returns `None` if the queue is closed and empty.
    pub fn recv(&self) -> RecvFuture<'_, T> {
        RecvFuture { queue: self }
    }
}

impl<T: DeserializeOwned + 'static> MessageReceiver for NetNotifiedQueue<T> {
    fn receive(&self, payload: &[u8]) {
        // Deserialize the message
        match serde_json::from_slice::<T>(payload) {
            Ok(message) => {
                sometimes_assert!(
                    deserialization_success,
                    true,
                    "Message deserialized successfully"
                );
                let mut inner = self.inner.borrow_mut();
                inner.queue.push_back(message);
                inner.messages_received += 1;

                // Wake all waiters
                let had_waiters = !inner.wakers.is_empty();
                for waker in inner.wakers.drain(..) {
                    waker.wake();
                }
                if had_waiters {
                    sometimes_assert!(waker_notified, true, "Wakers notified on new message");
                }
            }
            Err(e) => {
                sometimes_assert!(
                    deserialization_failed,
                    true,
                    "Message deserialization failed"
                );
                // Log error and drop the message
                tracing::warn!(
                    endpoint = %self.endpoint.token,
                    error = %e,
                    "failed to deserialize message"
                );
                self.inner.borrow_mut().messages_dropped += 1;
            }
        }
    }
}

/// Future returned by `recv()`.
pub struct RecvFuture<'a, T> {
    queue: &'a NetNotifiedQueue<T>,
}

impl<T> Future for RecvFuture<'_, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.queue.inner.borrow_mut();

        // Try to get a message
        if let Some(message) = inner.queue.pop_front() {
            sometimes_assert!(message_available, true, "Message available immediately");
            return Poll::Ready(Some(message));
        }

        // If closed and empty, return None
        if inner.closed {
            sometimes_assert!(queue_closed_empty, true, "Queue closed and empty");
            return Poll::Ready(None);
        }

        // Register waker and wait
        sometimes_assert!(recv_pending, true, "Recv waiting for message");
        inner.wakers.push(cx.waker().clone());
        Poll::Pending
    }
}

/// Wrapper for Rc<NetNotifiedQueue<T>> that can be registered with EndpointMap.
pub struct SharedNetNotifiedQueue<T: DeserializeOwned + 'static>(pub Rc<NetNotifiedQueue<T>>);

impl<T: DeserializeOwned + 'static> MessageReceiver for SharedNetNotifiedQueue<T> {
    fn receive(&self, payload: &[u8]) {
        self.0.receive(payload)
    }
}

impl<T: DeserializeOwned + 'static> SharedNetNotifiedQueue<T> {
    /// Create a new shared queue.
    pub fn new(endpoint: Endpoint) -> Self {
        Self(Rc::new(NetNotifiedQueue::new(endpoint)))
    }

    /// Get a reference to the inner queue.
    pub fn inner(&self) -> &NetNotifiedQueue<T> {
        &self.0
    }

    /// Get a clone of the Rc for registration with EndpointMap.
    pub fn as_receiver(&self) -> Rc<NetNotifiedQueue<T>> {
        Rc::clone(&self.0)
    }
}

/// Simple sequential ID generator for testing.
/// In production, use RandomProvider for deterministic IDs.
fn rand_simple_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    fn test_endpoint() -> Endpoint {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        Endpoint::new(addr, UID::new(1, 1))
    }

    #[test]
    fn test_new_queue_is_empty() {
        let queue: NetNotifiedQueue<String> = NetNotifiedQueue::new(test_endpoint());
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.messages_received(), 0);
    }

    #[test]
    fn test_push_and_try_recv() {
        let queue: NetNotifiedQueue<String> = NetNotifiedQueue::new(test_endpoint());

        queue.push("hello".to_string());
        assert!(!queue.is_empty());
        assert_eq!(queue.len(), 1);

        let msg = queue.try_recv();
        assert_eq!(msg, Some("hello".to_string()));
        assert!(queue.is_empty());
    }

    #[test]
    fn test_receive_deserializes() {
        let queue: NetNotifiedQueue<String> = NetNotifiedQueue::new(test_endpoint());

        // Receive a JSON-encoded string
        let payload = b"\"hello world\"";
        queue.receive(payload);

        assert_eq!(queue.len(), 1);
        assert_eq!(queue.messages_received(), 1);
        assert_eq!(queue.try_recv(), Some("hello world".to_string()));
    }

    #[test]
    fn test_receive_invalid_json_drops() {
        let queue: NetNotifiedQueue<String> = NetNotifiedQueue::new(test_endpoint());

        // Receive invalid JSON
        let payload = b"not valid json";
        queue.receive(payload);

        assert!(queue.is_empty());
        assert_eq!(queue.messages_received(), 0);
        assert_eq!(queue.messages_dropped(), 1);
    }

    #[test]
    fn test_fifo_ordering() {
        let queue: NetNotifiedQueue<i32> = NetNotifiedQueue::new(test_endpoint());

        queue.push(1);
        queue.push(2);
        queue.push(3);

        assert_eq!(queue.try_recv(), Some(1));
        assert_eq!(queue.try_recv(), Some(2));
        assert_eq!(queue.try_recv(), Some(3));
        assert_eq!(queue.try_recv(), None);
    }

    #[test]
    fn test_close_queue() {
        let queue: NetNotifiedQueue<String> = NetNotifiedQueue::new(test_endpoint());

        assert!(!queue.is_closed());
        queue.close();
        assert!(queue.is_closed());
    }

    #[test]
    fn test_endpoint_accessor() {
        let endpoint = test_endpoint();
        let queue: NetNotifiedQueue<String> = NetNotifiedQueue::new(endpoint.clone());

        assert_eq!(queue.endpoint().token, endpoint.token);
    }

    #[derive(Debug, PartialEq, serde::Deserialize)]
    struct TestMessage {
        id: u32,
        content: String,
    }

    #[test]
    fn test_receive_complex_type() {
        let queue: NetNotifiedQueue<TestMessage> = NetNotifiedQueue::new(test_endpoint());

        let payload = br#"{"id": 42, "content": "hello"}"#;
        queue.receive(payload);

        let msg = queue.try_recv();
        assert_eq!(
            msg,
            Some(TestMessage {
                id: 42,
                content: "hello".to_string()
            })
        );
    }

    #[test]
    fn test_shared_queue() {
        let shared: SharedNetNotifiedQueue<String> = SharedNetNotifiedQueue::new(test_endpoint());

        // Receive through the shared wrapper
        shared.receive(b"\"shared message\"");

        assert_eq!(
            shared.inner().try_recv(),
            Some("shared message".to_string())
        );
    }

    #[tokio::test]
    async fn test_recv_async() {
        let queue: NetNotifiedQueue<String> = NetNotifiedQueue::new(test_endpoint());

        // Push before recv - should complete immediately
        queue.push("async hello".to_string());

        let result = queue.recv().await;
        assert_eq!(result, Some("async hello".to_string()));
    }

    #[tokio::test]
    async fn test_recv_closed_empty() {
        let queue: NetNotifiedQueue<String> = NetNotifiedQueue::new(test_endpoint());

        queue.close();

        let result = queue.recv().await;
        assert_eq!(result, None);
    }
}
