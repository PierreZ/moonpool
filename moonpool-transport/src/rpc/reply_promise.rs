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

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::RwLock;

use crate::Endpoint;
use serde::Serialize;

use super::reply_error::ReplyError;
use super::transport_handle::EncodeFn;

/// Type alias for the sender function in ReplyPromise.
type ReplySender = Box<dyn FnOnce(&Endpoint, &[u8]) + Send + Sync>;

/// Promise for sending a reply to a request.
///
/// The server receives this along with each request and must fulfill it
/// with either `send(value)` or `send_error(error)`. If dropped without
/// fulfillment, sends [`ReplyError::BrokenPromise`] automatically. See
/// the `Drop` impl below for the exact serialization path.
///
/// # Type Safety
///
/// The response type `T` is fixed at compile time, ensuring type-safe
/// request-response pairs.
///
/// # Thread Safety
///
/// Uses `Arc<RwLock<…>>` internally — safe to clone across threads while
/// remaining cheap when held within the current-thread simulation runtime.
///
/// # See Also
///
/// [`ReplyFuture`] is the client-side counterpart that observes the
/// `BrokenPromise` error when this promise is dropped without fulfillment.
///
/// [`ReplyFuture`]: super::reply_future::ReplyFuture
pub struct ReplyPromise<T: Serialize> {
    inner: Arc<RwLock<ReplyPromiseInner<T>>>,
}

struct ReplyPromiseInner<T> {
    /// Where to send the reply.
    reply_endpoint: Endpoint,

    /// Type-erased encode function (captures a concrete codec).
    encode_fn: EncodeFn<Result<T, ReplyError>>,

    /// Sender function - sends serialized bytes to the endpoint.
    /// This is injected to decouple from NetTransport.
    sender: Option<ReplySender>,

    /// Whether the promise has been fulfilled.
    fulfilled: bool,

    /// Phantom for the response type.
    _phantom: PhantomData<T>,
}

impl<T: Serialize> ReplyPromise<T> {
    /// Create a new reply promise.
    ///
    /// The `sender` function will be called to deliver the serialized response.
    /// This is typically provided by the transport layer.
    pub fn new<F>(
        reply_endpoint: Endpoint,
        encode_fn: EncodeFn<Result<T, ReplyError>>,
        sender: F,
    ) -> Self
    where
        F: FnOnce(&Endpoint, &[u8]) + Send + Sync + 'static,
    {
        Self {
            inner: Arc::new(RwLock::new(ReplyPromiseInner {
                reply_endpoint,
                encode_fn,
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
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        if inner.fulfilled {
            // Already fulfilled (shouldn't happen due to move semantics)
            return;
        }

        // Serialize the response
        let result: Result<T, ReplyError> = Ok(value);
        match (inner.encode_fn)(&result) {
            Ok(payload) => {
                if let Some(sender) = inner.sender.take() {
                    sender(&inner.reply_endpoint, &payload);
                }
            }
            Err(e) => {
                // Serialization failed - send error instead
                tracing::error!(error = %e, "failed to serialize reply");
                let error_result: Result<T, ReplyError> = Err(ReplyError::Serialization {
                    message: e.to_string(),
                });
                if let Ok(error_payload) = (inner.encode_fn)(&error_result)
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
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        if inner.fulfilled {
            return;
        }

        let result: Result<T, ReplyError> = Err(error);
        if let Ok(payload) = (inner.encode_fn)(&result)
            && let Some(sender) = inner.sender.take()
        {
            sender(&inner.reply_endpoint, &payload);
        }

        inner.fulfilled = true;
    }
}

impl<T: Serialize> Drop for ReplyPromise<T> {
    fn drop(&mut self) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        if !inner.fulfilled {
            // Send BrokenPromise error to client
            let result: Result<T, ReplyError> = Err(ReplyError::BrokenPromise);
            if let Ok(payload) = (inner.encode_fn)(&result)
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
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Arc;
    use std::sync::RwLock;

    use crate::{JsonCodec, NetworkAddress, UID};
    use serde::{Deserialize, Serialize};

    use super::super::transport_handle::make_encode_fn;
    use super::*;

    fn test_endpoint() -> Endpoint {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        Endpoint::new(addr, UID::new(1, 1))
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestResponse {
        value: i32,
    }

    fn test_encode_fn() -> EncodeFn<Result<TestResponse, ReplyError>> {
        make_encode_fn(JsonCodec)
    }

    #[test]
    fn test_reply_promise_send() {
        let sent_data: Arc<RwLock<Option<Vec<u8>>>> = Arc::new(RwLock::new(None));
        let sent_clone = sent_data.clone();

        let promise: ReplyPromise<TestResponse> = ReplyPromise::new(
            test_endpoint(),
            test_encode_fn(),
            move |_endpoint, payload| {
                *sent_clone
                    .write()
                    .expect("RwLock poisoned: prior task panicked") = Some(payload.to_vec());
            },
        );

        promise.send(TestResponse { value: 42 });

        // Verify data was sent
        let sent = sent_data
            .read()
            .expect("RwLock poisoned: prior task panicked");
        assert!(sent.is_some());

        // Decode and verify
        let payload = sent.as_ref().expect("should have sent payload");
        let decoded: Result<TestResponse, ReplyError> =
            serde_json::from_slice(payload).expect("decode");
        assert_eq!(decoded, Ok(TestResponse { value: 42 }));
    }

    #[test]
    fn test_reply_promise_send_error() {
        let sent_data: Arc<RwLock<Option<Vec<u8>>>> = Arc::new(RwLock::new(None));
        let sent_clone = sent_data.clone();

        let promise: ReplyPromise<TestResponse> = ReplyPromise::new(
            test_endpoint(),
            test_encode_fn(),
            move |_endpoint, payload| {
                *sent_clone
                    .write()
                    .expect("RwLock poisoned: prior task panicked") = Some(payload.to_vec());
            },
        );

        promise.send_error(ReplyError::Timeout);

        let sent = sent_data
            .read()
            .expect("RwLock poisoned: prior task panicked");
        assert!(sent.is_some());

        let payload = sent.as_ref().expect("should have sent payload");
        let decoded: Result<TestResponse, ReplyError> =
            serde_json::from_slice(payload).expect("decode");
        assert_eq!(decoded, Err(ReplyError::Timeout));
    }

    #[test]
    fn test_reply_promise_broken_on_drop() {
        let sent_data: Arc<RwLock<Option<Vec<u8>>>> = Arc::new(RwLock::new(None));
        let sent_clone = sent_data.clone();

        {
            let _promise: ReplyPromise<TestResponse> = ReplyPromise::new(
                test_endpoint(),
                test_encode_fn(),
                move |_endpoint, payload| {
                    *sent_clone
                        .write()
                        .expect("RwLock poisoned: prior task panicked") = Some(payload.to_vec());
                },
            );
            // Promise dropped without send
        }

        // Should have sent BrokenPromise error
        let sent = sent_data
            .read()
            .expect("RwLock poisoned: prior task panicked");
        assert!(sent.is_some());

        let payload = sent.as_ref().expect("should have sent payload");
        let decoded: Result<TestResponse, ReplyError> =
            serde_json::from_slice(payload).expect("decode");
        assert_eq!(decoded, Err(ReplyError::BrokenPromise));
    }

    #[test]
    fn test_reply_promise_fulfilled_no_double_send_on_drop() {
        let send_count: Arc<RwLock<u32>> = Arc::new(RwLock::new(0));
        let count_clone = send_count.clone();

        {
            let promise: ReplyPromise<TestResponse> = ReplyPromise::new(
                test_endpoint(),
                test_encode_fn(),
                move |_endpoint, _payload| {
                    *count_clone
                        .write()
                        .expect("RwLock poisoned: prior task panicked") += 1;
                },
            );

            promise.send(TestResponse { value: 1 });
            // Promise dropped after send
        }

        // Should only have sent once
        assert_eq!(
            *send_count
                .read()
                .expect("RwLock poisoned: prior task panicked"),
            1
        );
    }
}
