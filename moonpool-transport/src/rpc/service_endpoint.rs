//! Client-side typed endpoint for making RPC calls.
//!
//! FDB equivalent: `RequestStream<T>` on the caller side (fdbrpc.h:726-948).
//!
//! A [`ServiceEndpoint`] is a typed wrapper around an [`Endpoint`] that provides
//! delivery mode methods matching FoundationDB's `getReply`, `tryGetReply`,
//! `send`, and `getReplyUnlessFailedFor`.
//!
//! # Example
//!
//! ```rust,ignore
//! let calc = CalculatorClient::from_base(server_addr, base_token, JsonCodec, &transport);
//!
//! // Each delivery mode is a call-site decision:
//! let resp = calc.add.get_reply(req).await?;
//! let resp = calc.add.try_get_reply(req).await?;
//! calc.add.send(req)?;
//! ```

use std::marker::PhantomData;
use std::rc::Rc;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::instrument;

use super::delivery;
use super::net_transport::NetTransport;
use super::reply_future::ReplyFuture;
use super::rpc_error::RpcError;
use crate::error::MessagingError;
use crate::{Endpoint, MessageCodec, Providers};

/// Client-side typed endpoint for making RPC calls.
///
/// Wraps an [`Endpoint`] with request/response type information, a codec, and
/// a bound transport. The transport is captured at construction so delivery
/// methods require only the request payload.
///
/// The `#[service]` macro generates client structs with `ServiceEndpoint`
/// fields — one per method in the interface. The codec and transport are passed
/// once at client construction and embedded in each endpoint.
///
/// # Delivery Modes
///
/// | Method | Guarantee | On Disconnect |
/// |--------|-----------|---------------|
/// | [`send`](Self::send) | Fire-and-forget | Silently lost |
/// | [`try_get_reply`](Self::try_get_reply) | At-most-once | `MaybeDelivered` |
/// | [`get_reply`](Self::get_reply) | At-least-once | Retransmits |
/// | [`get_reply_unless_failed_for`](Self::get_reply_unless_failed_for) | At-least-once + timeout | `MaybeDelivered` after duration |
///
/// # FDB Reference
/// `RequestStream<T>` (fdbrpc.h:726-948)
pub struct ServiceEndpoint<Req, Resp, C: MessageCodec, P: Providers> {
    endpoint: Endpoint,
    codec: C,
    transport: Rc<NetTransport<P>>,
    // fn(Req) -> Resp makes the type covariant in Resp and contravariant in Req,
    // matching the logical flow: we send Req and receive Resp.
    _phantom: PhantomData<fn(Req) -> Resp>,
}

// Manual trait impls because PhantomData<fn(Req) -> Resp> doesn't auto-derive.

impl<Req, Resp, C: MessageCodec + std::fmt::Debug, P: Providers> std::fmt::Debug
    for ServiceEndpoint<Req, Resp, C, P>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceEndpoint")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl<Req, Resp, C: MessageCodec + Clone, P: Providers> Clone for ServiceEndpoint<Req, Resp, C, P> {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            codec: self.codec.clone(),
            transport: Rc::clone(&self.transport),
            _phantom: PhantomData,
        }
    }
}

impl<Req, Resp, C: MessageCodec, P: Providers> Serialize for ServiceEndpoint<Req, Resp, C, P> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Only serialize the endpoint; codec and transport are runtime configuration.
        self.endpoint.serialize(serializer)
    }
}

impl<Req, Resp, C: MessageCodec, P: Providers> ServiceEndpoint<Req, Resp, C, P> {
    /// Create a new service endpoint from a raw endpoint, codec, and transport.
    pub fn new(endpoint: Endpoint, codec: C, transport: Rc<NetTransport<P>>) -> Self {
        Self {
            endpoint,
            codec,
            transport,
            _phantom: PhantomData,
        }
    }

    /// Get the underlying endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl<Req, Resp, C, P> ServiceEndpoint<Req, Resp, C, P>
where
    Req: Serialize,
    Resp: DeserializeOwned + 'static,
    C: MessageCodec + Clone,
    P: Providers,
{
    /// Fire-and-forget delivery: send request unreliably with no reply.
    ///
    /// The request is sent once via unreliable transport. No reply endpoint
    /// is registered, so any server response is silently discarded.
    ///
    /// Use for heartbeats, notifications, and messages where losing one
    /// is harmless (the next one compensates).
    ///
    /// # FDB Reference
    /// `RequestStream::send` (fdbrpc.h:733-738)
    ///
    /// # Errors
    ///
    /// Returns `RpcError::Messaging` if serialization or the send itself fails.
    #[instrument(skip_all)]
    pub fn send(&self, req: Req) -> Result<(), RpcError> {
        delivery::send(&*self.transport, &self.endpoint, req, self.codec.clone())
            .map_err(RpcError::from)
    }

    /// At-most-once delivery: send unreliably, race reply against disconnect.
    ///
    /// Returns `Ok(response)` if the server replies before disconnect, or
    /// `Err(RpcError::Reply(MaybeDelivered))` if the connection fails while
    /// the request is in flight.
    ///
    /// Callers must handle ambiguity — typically via read-before-retry or
    /// idempotent requests.
    ///
    /// # FDB Reference
    /// `RequestStream::tryGetReply` (fdbrpc.h:784-826)
    ///
    /// # Errors
    ///
    /// Returns `MaybeDelivered` on disconnect. Other `ReplyError` variants
    /// for serialization, timeouts, etc.
    #[instrument(skip_all)]
    pub async fn try_get_reply(&self, req: Req) -> Result<Resp, RpcError> {
        delivery::try_get_reply(&*self.transport, &self.endpoint, req, self.codec.clone())
            .await
            .map_err(RpcError::from)
    }

    /// At-least-once delivery: send reliably, retransmit on reconnect.
    ///
    /// The request is queued for reliable delivery and will be retransmitted
    /// if the connection drops and reconnects. The server may receive the
    /// same request multiple times — handlers must be idempotent or use
    /// generation numbers for deduplication.
    ///
    /// # FDB Reference
    /// `RequestStream::getReply` (fdbrpc.h:752-762)
    ///
    /// # Errors
    ///
    /// Returns `RpcError` if the request cannot be sent or the reply fails.
    #[instrument(skip_all)]
    pub async fn get_reply(&self, req: Req) -> Result<Resp, RpcError> {
        let future =
            delivery::get_reply(&*self.transport, &self.endpoint, req, self.codec.clone())?;
        Ok(future.await?)
    }

    /// At-least-once delivery with sustained failure timeout.
    ///
    /// Like [`get_reply`](Self::get_reply) but gives up if the endpoint has
    /// been continuously failed for `sustained_failure_duration`. Returns
    /// `MaybeDelivered` on timeout.
    ///
    /// Use for singleton RPCs (registration, recruitment) where you want
    /// reliable delivery but can't wait forever if the destination is
    /// permanently gone.
    ///
    /// # FDB Reference
    /// `RequestStream::getReplyUnlessFailedFor` (fdbrpc.h:870-895)
    ///
    /// # Errors
    ///
    /// Returns `MaybeDelivered` if the endpoint is failed for longer than
    /// `sustained_failure_duration`.
    #[instrument(skip_all)]
    pub async fn get_reply_unless_failed_for(
        &self,
        req: Req,
        sustained_failure_duration: Duration,
    ) -> Result<Resp, RpcError> {
        delivery::get_reply_unless_failed_for(
            &*self.transport,
            &self.endpoint,
            req,
            self.codec.clone(),
            sustained_failure_duration,
        )
        .await
        .map_err(RpcError::from)
    }

    /// Send a typed request and return a raw [`ReplyFuture`] for the response.
    ///
    /// This is the low-level API for cases where you need direct access to
    /// the reply future (e.g., for use in `tokio::select!`).
    ///
    /// For most use cases, prefer [`get_reply`](Self::get_reply) or
    /// [`try_get_reply`](Self::try_get_reply).
    ///
    /// # Errors
    ///
    /// Returns `MessagingError` if the request cannot be sent.
    #[instrument(skip_all)]
    pub fn send_request(&self, req: Req) -> Result<ReplyFuture<Resp, C>, MessagingError> {
        delivery::get_reply(&*self.transport, &self.endpoint, req, self.codec.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::rc::Rc;

    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::rpc::failure_monitor::FailureStatus;
    use crate::rpc::net_notified_queue::NetNotifiedQueue;
    use crate::rpc::reply_error::ReplyError;
    use crate::rpc::request_stream::RequestEnvelope;
    use crate::{
        JsonCodec, NetworkAddress, NetworkProvider, Providers, TokioRandomProvider,
        TokioStorageProvider, TokioTaskProvider, TokioTimeProvider, UID,
    };

    // ---- Test infrastructure ----

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

    fn make_transport() -> Rc<crate::NetTransport<MockProviders>> {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        crate::NetTransportBuilder::new(MockProviders::new())
            .local_address(addr)
            .build()
            .expect("build")
    }

    // ---- Test types ----

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct PingRequest {
        seq: u32,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct PingResponse {
        seq: u32,
    }

    // ---- Tests ----

    #[test]
    fn test_send_fire_and_forget() {
        let transport = make_transport();
        // Use local address so delivery goes through local dispatch (no peer creation)
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        let token = UID::new(0x1234, 0x0001);
        let endpoint = Endpoint::new(addr, token);

        let ep: ServiceEndpoint<PingRequest, PingResponse, JsonCodec, MockProviders> =
            ServiceEndpoint::new(endpoint, JsonCodec, Rc::clone(&transport));

        // send() to unregistered local endpoint returns EndpointNotFound
        let result = ep.send(PingRequest { seq: 1 });
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_try_get_reply_already_failed() {
        let transport = make_transport();
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5001);
        let token = UID::new(0x5678, 0x0001);
        let endpoint = Endpoint::new(addr.clone(), token);

        // Pre-fail the endpoint
        transport
            .failure_monitor()
            .set_status(&addr.to_string(), FailureStatus::Failed);

        let ep: ServiceEndpoint<PingRequest, PingResponse, JsonCodec, MockProviders> =
            ServiceEndpoint::new(endpoint, JsonCodec, Rc::clone(&transport));

        let result = ep.try_get_reply(PingRequest { seq: 2 }).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_reply_creates_future() {
        let transport = make_transport();
        let server_addr = transport.local_address().clone();
        let token = UID::new(0x9ABC, 0x0001);
        let server_endpoint = Endpoint::new(server_addr.clone(), token);

        // Register a server queue so dispatch works locally
        let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<PingRequest>, JsonCodec>> =
            Rc::new(NetNotifiedQueue::new(server_endpoint.clone(), JsonCodec));
        transport.register(
            token,
            Rc::clone(&server_queue) as Rc<dyn crate::MessageReceiver>,
        );

        let ep: ServiceEndpoint<PingRequest, PingResponse, JsonCodec, MockProviders> =
            ServiceEndpoint::new(server_endpoint, JsonCodec, Rc::clone(&transport));

        // get_reply sends a reliable request and returns a future
        let future = ep.send_request(PingRequest { seq: 3 });
        assert!(future.is_ok());

        // The server should have received the request
        let received = server_queue.try_recv();
        assert!(received.is_some());
        let envelope = received.expect("request envelope");
        assert_eq!(envelope.request.seq, 3);

        // Simulate server reply
        let reply_endpoint = envelope.reply_to;
        let response: Result<PingResponse, ReplyError> = Ok(PingResponse { seq: 3 });
        let payload = JsonCodec.encode(&response).expect("encode");
        transport
            .dispatch(&reply_endpoint.token, &payload)
            .expect("dispatch");

        // The future should resolve
        let result = future.expect("future").await;
        assert_eq!(result.expect("response"), PingResponse { seq: 3 });
    }
}
