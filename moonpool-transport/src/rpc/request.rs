//! Client-side request sending utilities.
//!
//! Provides the `send_request` function for sending typed requests and
//! receiving typed responses.
//!
//! # Example
//!
//! ```rust,ignore
//! use moonpool::{send_request, NetTransport, ReplyFuture};
//!
//! // Send a request and get a future for the response
//! let future: ReplyFuture<PingResponse, JsonCodec> = send_request(
//!     &transport,
//!     &server_endpoint,
//!     PingRequest { seq: 1 },
//!     JsonCodec,
//! )?;
//!
//! // Await the response
//! let response = future.await?;
//! ```

use std::rc::Rc;

use crate::{Endpoint, MessageCodec, NetworkProvider, TaskProvider, TimeProvider, UID};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::net_notified_queue::NetNotifiedQueue;
use super::net_transport::NetTransport;
use super::reply_error::ReplyError;
use super::reply_future::ReplyFuture;
use super::request_stream::RequestEnvelope;
use crate::error::MessagingError;

/// Send a typed request and return a future for the response.
///
/// This is the primary client-side RPC function. It:
/// 1. Creates a temporary endpoint for receiving the reply
/// 2. Wraps the request in a `RequestEnvelope` with the reply endpoint
/// 3. Sends the envelope to the destination
/// 4. Returns a `ReplyFuture` that resolves when the server responds
///
/// # Arguments
///
/// * `transport` - The NetTransport to use for sending
/// * `destination` - The server endpoint to send the request to
/// * `request` - The request payload
/// * `codec` - The codec for serialization
///
/// # Returns
///
/// A `ReplyFuture` that resolves to the response or an error.
///
/// # Errors
///
/// Returns `MessagingError` if the request cannot be sent.
pub fn send_request<Req, Resp, N, T, TP, C>(
    transport: &NetTransport<N, T, TP>,
    destination: &Endpoint,
    request: Req,
    codec: C,
) -> Result<ReplyFuture<Resp, C>, MessagingError>
where
    Req: Serialize,
    Resp: DeserializeOwned + 'static,
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    C: MessageCodec,
{
    // Generate a unique token for the reply endpoint
    // Note: In production, use RandomProvider for deterministic IDs
    let reply_token = UID::new(rand_reply_id(), rand_reply_id());

    // Create reply endpoint using transport's local address
    let reply_endpoint = Endpoint::new(transport.local_address().clone(), reply_token);

    // Create queue for receiving the reply
    let reply_queue: Rc<NetNotifiedQueue<Result<Resp, ReplyError>, C>> =
        Rc::new(NetNotifiedQueue::new(reply_endpoint.clone(), codec.clone()));

    // Register the reply queue with transport
    transport.register(reply_token, reply_queue.clone());

    // Create the request envelope
    let envelope = RequestEnvelope {
        request,
        reply_to: reply_endpoint.clone(),
    };

    // Serialize the envelope
    let payload = codec
        .encode(&envelope)
        .map_err(|e| MessagingError::SerializationFailed {
            message: e.to_string(),
        })?;

    // Send the request
    transport.send_unreliable(destination, &payload)?;

    // Return the reply future
    Ok(ReplyFuture::new(reply_queue, reply_endpoint))
}

/// Simple sequential ID generator for reply endpoints.
/// In production, use RandomProvider for deterministic IDs in simulation.
fn rand_reply_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0x1_0000_0000);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {

    use std::net::{IpAddr, Ipv4Addr};
    use std::rc::Rc;

    use crate::{JsonCodec, NetworkAddress, TokioTaskProvider, TokioTimeProvider, UID};
    use serde::{Deserialize, Serialize};

    use super::*;

    // Mock network provider for testing
    #[derive(Clone)]
    struct MockNetworkProvider;

    struct DummyStream;

    impl tokio::io::AsyncRead for DummyStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
        }
    }

    impl tokio::io::AsyncWrite for DummyStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
        }
    }

    impl std::marker::Unpin for DummyStream {}

    struct DummyListener;

    #[async_trait::async_trait(?Send)]
    impl crate::TcpListenerTrait for DummyListener {
        type TcpStream = DummyStream;

        async fn accept(&self) -> std::io::Result<(Self::TcpStream, String)> {
            Err(std::io::Error::other("dummy"))
        }

        fn local_addr(&self) -> std::io::Result<String> {
            Err(std::io::Error::other("dummy"))
        }
    }

    #[async_trait::async_trait(?Send)]
    impl NetworkProvider for MockNetworkProvider {
        type TcpStream = DummyStream;
        type TcpListener = DummyListener;

        async fn bind(&self, _addr: &str) -> std::io::Result<Self::TcpListener> {
            Err(std::io::Error::other("mock"))
        }

        async fn connect(&self, _addr: &str) -> std::io::Result<Self::TcpStream> {
            Err(std::io::Error::other("mock"))
        }
    }

    fn test_address() -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500)
    }

    fn create_test_transport()
    -> NetTransport<MockNetworkProvider, TokioTimeProvider, TokioTaskProvider> {
        NetTransport::new(
            test_address(),
            MockNetworkProvider,
            TokioTimeProvider::new(),
            TokioTaskProvider,
        )
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct PingRequest {
        seq: u32,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct PingResponse {
        seq: u32,
    }

    #[test]
    fn test_send_request_creates_future() {
        let transport = create_test_transport();

        // Register a server endpoint to receive the request
        let server_token = UID::new(0x1234, 0x5678);
        let server_endpoint = Endpoint::new(test_address(), server_token);

        let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<PingRequest>, JsonCodec>> =
            Rc::new(NetNotifiedQueue::new(server_endpoint.clone(), JsonCodec));

        transport.register(server_token, server_queue.clone());

        // Send request
        let result = send_request(
            &transport,
            &server_endpoint,
            PingRequest { seq: 42 },
            JsonCodec,
        );

        assert!(result.is_ok());
        let _future: ReplyFuture<PingResponse, JsonCodec> = result.expect("should create future");

        // The reply endpoint should be registered
        assert!(transport.endpoint_count() >= 2); // server + reply

        // The server should have received the envelope
        assert_eq!(server_queue.len(), 1);

        // Verify the envelope
        let envelope = server_queue.try_recv().expect("should receive envelope");
        assert_eq!(envelope.request, PingRequest { seq: 42 });
        assert!(envelope.reply_to.token.is_valid());
    }

    #[tokio::test]
    async fn test_send_request_roundtrip() {
        let transport = create_test_transport();

        // Server endpoint
        let server_token = UID::new(0x1234, 0x5678);
        let server_endpoint = Endpoint::new(test_address(), server_token);

        let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<PingRequest>, JsonCodec>> =
            Rc::new(NetNotifiedQueue::new(server_endpoint.clone(), JsonCodec));

        transport.register(server_token, server_queue.clone());

        // Send request
        let future: ReplyFuture<PingResponse, JsonCodec> = send_request(
            &transport,
            &server_endpoint,
            PingRequest { seq: 99 },
            JsonCodec,
        )
        .expect("send_request should succeed");

        // Server receives and responds
        let envelope = server_queue.try_recv().expect("should receive request");
        assert_eq!(envelope.request, PingRequest { seq: 99 });

        // Simulate server sending response back to client's reply endpoint
        let response: Result<PingResponse, ReplyError> = Ok(PingResponse { seq: 99 });
        let response_payload = serde_json::to_vec(&response).expect("serialize response");

        // Send directly to the reply endpoint via dispatch
        transport
            .dispatch(&envelope.reply_to.token, &response_payload)
            .expect("dispatch should succeed");

        // Await the response
        let result = future.await;
        assert_eq!(result, Ok(PingResponse { seq: 99 }));
    }
}
