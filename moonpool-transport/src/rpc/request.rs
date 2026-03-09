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

use crate::{Endpoint, MessageCodec, Providers, RandomProvider, UID};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::net_notified_queue::{NetNotifiedQueue, ReplyQueueCloser};
use super::net_transport::NetTransport;
use super::reply_error::ReplyError;
use super::reply_future::ReplyFuture;
use super::request_stream::RequestEnvelope;
use crate::error::MessagingError;

/// Send a typed request and return a future for the response.
///
/// This is the primary client-side RPC function using **reliable delivery**
/// (at-least-once). It:
/// 1. Creates a temporary endpoint for receiving the reply
/// 2. Wraps the request in a `RequestEnvelope` with the reply endpoint
/// 3. Sends the envelope reliably (survives reconnection)
/// 4. Returns a `ReplyFuture` that resolves when the server responds
///
/// For delivery mode functions with different guarantees, see
/// [`get_reply`](super::delivery::get_reply),
/// [`try_get_reply`](super::delivery::try_get_reply), and
/// [`send`](super::delivery::send).
///
/// # Errors
///
/// Returns `MessagingError` if the request cannot be sent.
pub fn send_request<Req, Resp, P, C>(
    transport: &NetTransport<P>,
    destination: &Endpoint,
    request: Req,
    codec: C,
) -> Result<ReplyFuture<Resp, C>, MessagingError>
where
    Req: Serialize,
    Resp: DeserializeOwned + 'static,
    P: Providers,
    C: MessageCodec,
{
    prepare_and_send(transport, destination, request, codec, |t, ep, payload| {
        t.send_reliable(ep, payload)
    })
}

/// Send a typed request **unreliably** and return a future for the response.
///
/// Same as [`send_request`] but uses unreliable transport — the request is
/// dropped on connection failure instead of being retransmitted. Used by
/// [`try_get_reply`](super::delivery::try_get_reply) for at-most-once semantics.
///
/// # FDB Reference
/// `sendUnreliable` in `tryGetReply` (fdbrpc.h:797)
pub(crate) fn send_request_unreliable<Req, Resp, P, C>(
    transport: &NetTransport<P>,
    destination: &Endpoint,
    request: Req,
    codec: C,
) -> Result<ReplyFuture<Resp, C>, MessagingError>
where
    Req: Serialize,
    Resp: DeserializeOwned + 'static,
    P: Providers,
    C: MessageCodec,
{
    prepare_and_send(transport, destination, request, codec, |t, ep, payload| {
        t.send_unreliable(ep, payload)
    })
}

/// Shared request preparation: create reply queue, serialize envelope, send via
/// caller-provided function, register pending reply for disconnect cleanup.
fn prepare_and_send<Req, Resp, P, C, F>(
    transport: &NetTransport<P>,
    destination: &Endpoint,
    request: Req,
    codec: C,
    send_fn: F,
) -> Result<ReplyFuture<Resp, C>, MessagingError>
where
    Req: Serialize,
    Resp: DeserializeOwned + 'static,
    P: Providers,
    C: MessageCodec,
    F: FnOnce(&NetTransport<P>, &Endpoint, &[u8]) -> Result<(), MessagingError>,
{
    let reply_token = UID::new(
        transport.random().random::<u64>(),
        transport.random().random::<u64>(),
    );

    let reply_endpoint = Endpoint::new(transport.local_address().clone(), reply_token);

    let reply_queue: Rc<NetNotifiedQueue<Result<Resp, ReplyError>, C>> =
        Rc::new(NetNotifiedQueue::new(reply_endpoint.clone(), codec.clone()));

    transport.register(reply_token, reply_queue.clone());

    let envelope = RequestEnvelope {
        request,
        reply_to: reply_endpoint.clone(),
    };

    let payload = codec
        .encode(&envelope)
        .map_err(|e| MessagingError::SerializationFailed {
            message: e.to_string(),
        })?;

    send_fn(transport, destination, &payload)?;

    // Track this reply queue for disconnect cleanup (skip local sends)
    if !transport.is_local_address(&destination.address) {
        let closer: Rc<dyn ReplyQueueCloser> = reply_queue.clone();
        transport.register_pending_reply(
            &destination.address.to_string(),
            reply_token,
            Rc::downgrade(&closer),
        );
    }

    Ok(ReplyFuture::new(reply_queue, reply_endpoint))
}

#[cfg(test)]
mod tests {

    use std::net::{IpAddr, Ipv4Addr};
    use std::rc::Rc;

    use crate::{
        JsonCodec, NetworkAddress, NetworkProvider, Providers, TokioRandomProvider,
        TokioStorageProvider, TokioTaskProvider, TokioTimeProvider, UID,
    };
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

    /// Mock providers bundle for testing
    #[derive(Clone)]
    struct MockProviders {
        network: MockNetworkProvider,
        time: TokioTimeProvider,
        task: TokioTaskProvider,
        random: TokioRandomProvider,
        storage: TokioStorageProvider,
    }

    impl MockProviders {
        fn new() -> Self {
            Self {
                network: MockNetworkProvider,
                time: TokioTimeProvider::new(),
                task: TokioTaskProvider,
                random: TokioRandomProvider::new(),
                storage: TokioStorageProvider::new(),
            }
        }
    }

    impl Providers for MockProviders {
        type Network = MockNetworkProvider;
        type Time = TokioTimeProvider;
        type Task = TokioTaskProvider;
        type Random = TokioRandomProvider;
        type Storage = TokioStorageProvider;

        fn network(&self) -> &Self::Network {
            &self.network
        }
        fn time(&self) -> &Self::Time {
            &self.time
        }
        fn task(&self) -> &Self::Task {
            &self.task
        }
        fn random(&self) -> &Self::Random {
            &self.random
        }
        fn storage(&self) -> &Self::Storage {
            &self.storage
        }
    }

    fn test_address() -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500)
    }

    fn create_test_transport() -> NetTransport<MockProviders> {
        NetTransport::new(test_address(), MockProviders::new())
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
