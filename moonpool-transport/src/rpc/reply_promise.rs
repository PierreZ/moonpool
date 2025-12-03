//! ReplyPromise: Server-side promise for sending responses.
//!
//! When a server receives a request via [`RequestStream`], it gets a `ReplyPromise`
//! that must be fulfilled with either a success value or an error. If the promise
//! is dropped without being fulfilled, a [`ReplyError::BrokenPromise`] is automatically
//! sent to the client.
//!
//! # FDB Reference
//! Based on `ReplyPromise<T>` from fdbrpc.h lines 130-200.
//!
//! # Example
//!
//! ```rust,ignore
//! // Immediate reply (common case)
//! async fn handle_request(req: PingRequest, reply: ReplyPromise<PingResponse>) {
//!     reply.send(PingResponse { pong: req.ping });
//! }
//!
//! // Deferred reply (for long-running operations)
//! async fn handle_watch(req: WatchRequest, reply: ReplyPromise<WatchResponse>) {
//!     // Store promise for later fulfillment
//!     watches.insert(req.key, reply);
//!     // Reply will be sent when the watched key changes
//! }
//! ```

use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::{Endpoint, MessageCodec};
use moonpool_sim::sometimes_assert;
use serde::Serialize;

use super::reply_error::ReplyError;

/// Type alias for the sender function in ReplyPromise.
type ReplySender = Box<dyn FnOnce(&Endpoint, &[u8])>;

/// Promise for sending a reply to a request.
///
/// The server receives this along with each request and must fulfill it
/// with either `send(value)` or `send_error(error)`. If dropped without
/// fulfillment, sends [`ReplyError::BrokenPromise`] automatically.
///
/// # Type Safety
///
/// The response type `T` is fixed at compile time, ensuring type-safe
/// request-response pairs.
///
/// # Single-Threaded
///
/// Uses `Rc<RefCell<>>` internally - not thread-safe but efficient for
/// single-threaded async runtimes.
pub struct ReplyPromise<T: Serialize, C: MessageCodec> {
    inner: Rc<RefCell<ReplyPromiseInner<T, C>>>,
}

struct ReplyPromiseInner<T, C: MessageCodec> {
    /// Where to send the reply.
    reply_endpoint: Endpoint,

    /// Codec for serialization.
    codec: C,

    /// Sender function - sends serialized bytes to the endpoint.
    /// This is injected to decouple from FlowTransport.
    sender: Option<ReplySender>,

    /// Whether the promise has been fulfilled.
    fulfilled: bool,

    /// Phantom for the response type.
    _phantom: PhantomData<T>,
}

impl<T: Serialize, C: MessageCodec> ReplyPromise<T, C> {
    /// Create a new reply promise.
    ///
    /// The `sender` function will be called to deliver the serialized response.
    /// This is typically provided by the transport layer.
    pub fn new<F>(reply_endpoint: Endpoint, codec: C, sender: F) -> Self
    where
        F: FnOnce(&Endpoint, &[u8]) + 'static,
    {
        Self {
            inner: Rc::new(RefCell::new(ReplyPromiseInner {
                reply_endpoint,
                codec,
                sender: Some(Box::new(sender)),
                fulfilled: false,
                _phantom: PhantomData,
            })),
        }
    }

    /// Send a successful response.
    ///
    /// Consumes the promise, preventing double-send.
    pub fn send(self, value: T) {
        let mut inner = self.inner.borrow_mut();
        if inner.fulfilled {
            // Already fulfilled (shouldn't happen due to move semantics)
            return;
        }

        // Serialize the response
        let result: Result<T, ReplyError> = Ok(value);
        match inner.codec.encode(&result) {
            Ok(payload) => {
                if let Some(sender) = inner.sender.take() {
                    sender(&inner.reply_endpoint, &payload);
                    sometimes_assert!(reply_sent, true, "Reply successfully sent");
                }
            }
            Err(e) => {
                // Serialization failed - send error instead
                tracing::error!(error = %e, "failed to serialize reply");
                let error_result: Result<T, ReplyError> = Err(ReplyError::Serialization {
                    message: e.to_string(),
                });
                if let Ok(error_payload) = inner.codec.encode(&error_result)
                    && let Some(sender) = inner.sender.take()
                {
                    sender(&inner.reply_endpoint, &error_payload);
                }
            }
        }

        inner.fulfilled = true;
    }

    /// Send an error response.
    ///
    /// Consumes the promise, preventing double-send.
    pub fn send_error(self, error: ReplyError) {
        let mut inner = self.inner.borrow_mut();
        if inner.fulfilled {
            return;
        }

        let result: Result<T, ReplyError> = Err(error);
        if let Ok(payload) = inner.codec.encode(&result)
            && let Some(sender) = inner.sender.take()
        {
            sender(&inner.reply_endpoint, &payload);
        }

        inner.fulfilled = true;
    }

    /// Check if the promise has been fulfilled.
    pub fn is_fulfilled(&self) -> bool {
        self.inner.borrow().fulfilled
    }

    /// Get the reply endpoint.
    pub fn endpoint(&self) -> Endpoint {
        self.inner.borrow().reply_endpoint.clone()
    }
}

impl<T: Serialize, C: MessageCodec> Drop for ReplyPromise<T, C> {
    fn drop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        if !inner.fulfilled {
            sometimes_assert!(
                broken_promise_detected,
                true,
                "Promise dropped without reply"
            );

            // Send BrokenPromise error to client
            let result: Result<T, ReplyError> = Err(ReplyError::BrokenPromise);
            if let Ok(payload) = inner.codec.encode(&result)
                && let Some(sender) = inner.sender.take()
            {
                sender(&inner.reply_endpoint, &payload);
                tracing::warn!(
                    endpoint = %inner.reply_endpoint.token,
                    "ReplyPromise dropped without fulfillment - sent BrokenPromise"
                );
            }
            inner.fulfilled = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::net::{IpAddr, Ipv4Addr};
    use std::rc::Rc;

    use crate::{JsonCodec, NetworkAddress, UID};
    use serde::{Deserialize, Serialize};

    use super::*;

    fn test_endpoint() -> Endpoint {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        Endpoint::new(addr, UID::new(1, 1))
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestResponse {
        value: i32,
    }

    #[test]
    fn test_reply_promise_send() {
        let sent_data: Rc<RefCell<Option<Vec<u8>>>> = Rc::new(RefCell::new(None));
        let sent_clone = sent_data.clone();

        let promise: ReplyPromise<TestResponse, JsonCodec> =
            ReplyPromise::new(test_endpoint(), JsonCodec, move |_endpoint, payload| {
                *sent_clone.borrow_mut() = Some(payload.to_vec());
            });

        assert!(!promise.is_fulfilled());

        promise.send(TestResponse { value: 42 });

        // Verify data was sent
        let sent = sent_data.borrow();
        assert!(sent.is_some());

        // Decode and verify
        let payload = sent.as_ref().expect("should have sent payload");
        let decoded: Result<TestResponse, ReplyError> =
            serde_json::from_slice(payload).expect("decode");
        assert_eq!(decoded, Ok(TestResponse { value: 42 }));
    }

    #[test]
    fn test_reply_promise_send_error() {
        let sent_data: Rc<RefCell<Option<Vec<u8>>>> = Rc::new(RefCell::new(None));
        let sent_clone = sent_data.clone();

        let promise: ReplyPromise<TestResponse, JsonCodec> =
            ReplyPromise::new(test_endpoint(), JsonCodec, move |_endpoint, payload| {
                *sent_clone.borrow_mut() = Some(payload.to_vec());
            });

        promise.send_error(ReplyError::Timeout);

        let sent = sent_data.borrow();
        assert!(sent.is_some());

        let payload = sent.as_ref().expect("should have sent payload");
        let decoded: Result<TestResponse, ReplyError> =
            serde_json::from_slice(payload).expect("decode");
        assert_eq!(decoded, Err(ReplyError::Timeout));
    }

    #[test]
    fn test_reply_promise_broken_on_drop() {
        let sent_data: Rc<RefCell<Option<Vec<u8>>>> = Rc::new(RefCell::new(None));
        let sent_clone = sent_data.clone();

        {
            let _promise: ReplyPromise<TestResponse, JsonCodec> =
                ReplyPromise::new(test_endpoint(), JsonCodec, move |_endpoint, payload| {
                    *sent_clone.borrow_mut() = Some(payload.to_vec());
                });
            // Promise dropped without send
        }

        // Should have sent BrokenPromise error
        let sent = sent_data.borrow();
        assert!(sent.is_some());

        let payload = sent.as_ref().expect("should have sent payload");
        let decoded: Result<TestResponse, ReplyError> =
            serde_json::from_slice(payload).expect("decode");
        assert_eq!(decoded, Err(ReplyError::BrokenPromise));
    }

    #[test]
    fn test_reply_promise_fulfilled_no_double_send_on_drop() {
        let send_count: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));
        let count_clone = send_count.clone();

        {
            let promise: ReplyPromise<TestResponse, JsonCodec> =
                ReplyPromise::new(test_endpoint(), JsonCodec, move |_endpoint, _payload| {
                    *count_clone.borrow_mut() += 1;
                });

            promise.send(TestResponse { value: 1 });
            // Promise dropped after send
        }

        // Should only have sent once
        assert_eq!(*send_count.borrow(), 1);
    }

    #[test]
    fn test_reply_promise_endpoint() {
        let endpoint = test_endpoint();
        let promise: ReplyPromise<TestResponse, JsonCodec> =
            ReplyPromise::new(endpoint.clone(), JsonCodec, |_, _| {});

        assert_eq!(promise.endpoint().token, endpoint.token);
    }
}
