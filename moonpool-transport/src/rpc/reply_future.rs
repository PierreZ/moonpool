//! ReplyFuture: Client-side future waiting for server response.
//!
//! When a client sends a request via [`send_request`], it receives a `ReplyFuture`
//! that resolves when the server responds or an error occurs.
//!
//! # FDB Reference
//! Based on the reply side of `ReplyPromise<T>` and `Future<T>` from fdbrpc.h.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::{Endpoint, MessageCodec};
use moonpool_sim::assert_sometimes;
use serde::de::DeserializeOwned;

use super::net_notified_queue::NetNotifiedQueue;
use super::reply_error::ReplyError;

/// Future that resolves when a reply is received from the server.
///
/// Created by `send_request` and polls an internal queue for the response.
/// The response is deserialized as `Result<T, ReplyError>` to handle both
/// success and error cases.
pub struct ReplyFuture<T: DeserializeOwned, C: MessageCodec> {
    /// Queue receiving the reply.
    queue: Rc<NetNotifiedQueue<Result<T, ReplyError>, C>>,

    /// The endpoint this future is listening on.
    endpoint: Endpoint,
}

impl<T: DeserializeOwned, C: MessageCodec> ReplyFuture<T, C> {
    /// Create a new reply future with the given queue and endpoint.
    pub fn new(queue: Rc<NetNotifiedQueue<Result<T, ReplyError>, C>>, endpoint: Endpoint) -> Self {
        Self { queue, endpoint }
    }

    /// Get the endpoint this future is listening on.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl<T: DeserializeOwned, C: MessageCodec> Future for ReplyFuture<T, C> {
    type Output = Result<T, ReplyError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Try to receive from the queue (non-blocking)
        if let Some(result) = self.queue.try_recv() {
            assert_sometimes!(true, "Reply received from server");
            return Poll::Ready(result);
        }

        // Check if queue is closed (connection failed)
        if self.queue.is_closed() {
            assert_sometimes!(true, "Reply queue closed before response");
            return Poll::Ready(Err(ReplyError::ConnectionFailed));
        }

        // Poll the recv future to register the waker
        // We create a recv future each time to register the waker
        let mut recv_future = Box::pin(self.queue.recv());
        match recv_future.as_mut().poll(cx) {
            Poll::Ready(Some(result)) => {
                assert_sometimes!(true, "Reply received from server");
                Poll::Ready(result)
            }
            Poll::Ready(None) => {
                assert_sometimes!(true, "Reply queue closed before response");
                Poll::Ready(Err(ReplyError::ConnectionFailed))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use crate::{JsonCodec, NetworkAddress, UID};
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::MessageReceiver;

    fn test_endpoint() -> Endpoint {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        Endpoint::new(addr, UID::new(1, 1))
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestResponse {
        value: i32,
    }

    #[tokio::test]
    async fn test_reply_future_success() {
        let endpoint = test_endpoint();
        let queue: Rc<NetNotifiedQueue<Result<TestResponse, ReplyError>, JsonCodec>> =
            Rc::new(NetNotifiedQueue::new(endpoint.clone(), JsonCodec));

        let future = ReplyFuture::new(queue.clone(), endpoint);

        // Simulate server response
        let response: Result<TestResponse, ReplyError> = Ok(TestResponse { value: 42 });
        let payload = serde_json::to_vec(&response).expect("serialize response");
        queue.receive(&payload);

        // Await the future
        let result = future.await;
        assert_eq!(result, Ok(TestResponse { value: 42 }));
    }

    #[tokio::test]
    async fn test_reply_future_error() {
        let endpoint = test_endpoint();
        let queue: Rc<NetNotifiedQueue<Result<TestResponse, ReplyError>, JsonCodec>> =
            Rc::new(NetNotifiedQueue::new(endpoint.clone(), JsonCodec));

        let future = ReplyFuture::new(queue.clone(), endpoint);

        // Simulate server error response
        let response: Result<TestResponse, ReplyError> = Err(ReplyError::BrokenPromise);
        let payload = serde_json::to_vec(&response).expect("serialize response");
        queue.receive(&payload);

        let result = future.await;
        assert_eq!(result, Err(ReplyError::BrokenPromise));
    }

    #[tokio::test]
    async fn test_reply_future_connection_failed() {
        let endpoint = test_endpoint();
        let queue: Rc<NetNotifiedQueue<Result<TestResponse, ReplyError>, JsonCodec>> =
            Rc::new(NetNotifiedQueue::new(endpoint.clone(), JsonCodec));

        let future = ReplyFuture::new(queue.clone(), endpoint);

        // Close the queue to simulate connection failure
        queue.close();

        let result = future.await;
        assert_eq!(result, Err(ReplyError::ConnectionFailed));
    }

    #[test]
    fn test_reply_future_endpoint() {
        let endpoint = test_endpoint();
        let queue: Rc<NetNotifiedQueue<Result<TestResponse, ReplyError>, JsonCodec>> =
            Rc::new(NetNotifiedQueue::new(endpoint.clone(), JsonCodec));

        let future = ReplyFuture::new(queue, endpoint.clone());
        assert_eq!(future.endpoint().token, endpoint.token);
    }
}
