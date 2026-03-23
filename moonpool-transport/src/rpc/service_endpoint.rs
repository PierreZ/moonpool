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
//! let calc = CalculatorClient::new(server_addr, JsonCodec);
//!
//! // Each delivery mode is a call-site decision:
//! let resp = calc.add.get_reply(&transport, req).await?;
//! let resp = calc.add.try_get_reply(&transport, req).await?;
//! calc.add.send(&transport, req)?;
//! ```

use std::marker::PhantomData;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use super::delivery;
use super::net_transport::NetTransport;
use super::reply_future::ReplyFuture;
use super::rpc_error::RpcError;
use crate::error::MessagingError;
use crate::{Endpoint, MessageCodec, Providers};

/// Client-side typed endpoint for making RPC calls.
///
/// Wraps an [`Endpoint`] with request/response type information and a codec,
/// providing delivery mode methods directly on the handle. This is the primary
/// API for making RPC calls in moonpool.
///
/// The `#[service]` macro generates client structs with `ServiceEndpoint`
/// fields — one per method in the interface. The codec is passed once at
/// client construction and embedded in each endpoint.
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
pub struct ServiceEndpoint<Req, Resp, C: MessageCodec> {
    endpoint: Endpoint,
    codec: C,
    // fn(Req) -> Resp makes the type covariant in Resp and contravariant in Req,
    // matching the logical flow: we send Req and receive Resp.
    _phantom: PhantomData<fn(Req) -> Resp>,
}

// Manual trait impls because PhantomData<fn(Req) -> Resp> doesn't auto-derive.

impl<Req, Resp, C: MessageCodec + std::fmt::Debug> std::fmt::Debug
    for ServiceEndpoint<Req, Resp, C>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceEndpoint")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl<Req, Resp, C: MessageCodec + Clone> Clone for ServiceEndpoint<Req, Resp, C> {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            codec: self.codec.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<Req, Resp, C: MessageCodec> Serialize for ServiceEndpoint<Req, Resp, C> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Only serialize the endpoint; codec is runtime configuration.
        self.endpoint.serialize(serializer)
    }
}

impl<'de, Req, Resp, C: MessageCodec + Default> Deserialize<'de> for ServiceEndpoint<Req, Resp, C> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let endpoint = Endpoint::deserialize(deserializer)?;
        Ok(Self::new(endpoint, C::default()))
    }
}

impl<Req, Resp, C: MessageCodec> ServiceEndpoint<Req, Resp, C> {
    /// Create a new service endpoint from a raw endpoint and codec.
    pub fn new(endpoint: Endpoint, codec: C) -> Self {
        Self {
            endpoint,
            codec,
            _phantom: PhantomData,
        }
    }

    /// Get the underlying endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl<Req, Resp, C> ServiceEndpoint<Req, Resp, C>
where
    Req: Serialize,
    Resp: DeserializeOwned + 'static,
    C: MessageCodec + Clone,
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
    pub fn send<P: Providers>(
        &self,
        transport: &NetTransport<P>,
        req: Req,
    ) -> Result<(), RpcError> {
        delivery::send(transport, &self.endpoint, req, self.codec.clone()).map_err(RpcError::from)
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
    pub async fn try_get_reply<P: Providers>(
        &self,
        transport: &NetTransport<P>,
        req: Req,
    ) -> Result<Resp, RpcError> {
        delivery::try_get_reply(transport, &self.endpoint, req, self.codec.clone())
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
    pub async fn get_reply<P: Providers>(
        &self,
        transport: &NetTransport<P>,
        req: Req,
    ) -> Result<Resp, RpcError> {
        let future = delivery::get_reply(transport, &self.endpoint, req, self.codec.clone())?;
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
    pub async fn get_reply_unless_failed_for<P: Providers>(
        &self,
        transport: &NetTransport<P>,
        req: Req,
        sustained_failure_duration: Duration,
    ) -> Result<Resp, RpcError> {
        delivery::get_reply_unless_failed_for(
            transport,
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
    pub fn send_request<P: Providers>(
        &self,
        transport: &NetTransport<P>,
        req: Req,
    ) -> Result<ReplyFuture<Resp, C>, MessagingError> {
        delivery::get_reply(transport, &self.endpoint, req, self.codec.clone())
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
    struct TestRequest {
        value: u32,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestResponse {
        value: u32,
    }

    // ---- Tests ----

    #[test]
    fn test_service_endpoint_debug_clone() {
        let ep: ServiceEndpoint<TestRequest, TestResponse, JsonCodec> = ServiceEndpoint::new(
            Endpoint::new(test_address(), UID::new(0xCA1C_0000, 1)),
            JsonCodec,
        );

        let cloned = ep.clone();
        assert_eq!(format!("{:?}", ep), format!("{:?}", cloned));
    }

    #[test]
    fn test_service_endpoint_serde_roundtrip() {
        let ep: ServiceEndpoint<TestRequest, TestResponse, JsonCodec> = ServiceEndpoint::new(
            Endpoint::new(test_address(), UID::new(0xCA1C_0000, 1)),
            JsonCodec,
        );

        let json = serde_json::to_string(&ep).expect("serialize");
        let decoded: ServiceEndpoint<TestRequest, TestResponse, JsonCodec> =
            serde_json::from_str(&json).expect("deserialize");

        assert_eq!(ep.endpoint().token, decoded.endpoint().token);
        assert_eq!(ep.endpoint().address, decoded.endpoint().address);
    }

    #[test]
    fn test_send_fire_and_forget() {
        let transport = create_test_transport();

        let server_token = UID::new(0x1234, 0x5678);
        let server_endpoint = Endpoint::new(test_address(), server_token);

        let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<TestRequest>, JsonCodec>> =
            Rc::new(NetNotifiedQueue::new(server_endpoint.clone(), JsonCodec));
        transport.register(server_token, server_queue.clone());

        let ep: ServiceEndpoint<TestRequest, TestResponse, JsonCodec> =
            ServiceEndpoint::new(server_endpoint, JsonCodec);

        ep.send(&transport, TestRequest { value: 42 })
            .expect("send should succeed");

        let envelope = server_queue.try_recv().expect("should receive envelope");
        assert_eq!(envelope.request, TestRequest { value: 42 });
    }

    #[tokio::test]
    async fn test_try_get_reply_already_failed() {
        let transport = create_test_transport();

        let ep: ServiceEndpoint<TestRequest, TestResponse, JsonCodec> = ServiceEndpoint::new(
            Endpoint::new(test_address(), UID::new(0x1234, 0x5678)),
            JsonCodec,
        );

        // Address unknown → Failed by default → immediate MaybeDelivered
        let result = ep.try_get_reply(&transport, TestRequest { value: 1 }).await;

        let err = result.expect_err("should be MaybeDelivered");
        assert!(err.is_maybe_delivered());
    }

    #[test]
    fn test_get_reply_creates_future() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("build runtime");
        rt.block_on(async {
            let transport = Rc::new(create_test_transport());

            let server_token = UID::new(0x1234, 0x5678);
            let server_endpoint = Endpoint::new(test_address(), server_token);

            let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<TestRequest>, JsonCodec>> =
                Rc::new(NetNotifiedQueue::new(server_endpoint.clone(), JsonCodec));
            transport.register(server_token, server_queue.clone());

            transport
                .failure_monitor()
                .set_status("10.0.0.1:4500", FailureStatus::Available);

            let ep: ServiceEndpoint<TestRequest, TestResponse, JsonCodec> =
                ServiceEndpoint::new(server_endpoint, JsonCodec);

            let t = Rc::clone(&transport);
            let handle = tokio::task::spawn_local(async move {
                ep.get_reply(&t, TestRequest { value: 99 }).await
            });

            tokio::task::yield_now().await;

            // Server responds
            let envelope = server_queue.try_recv().expect("should receive request");
            let response: Result<TestResponse, ReplyError> = Ok(TestResponse { value: 99 });
            let response_payload = serde_json::to_vec(&response).expect("serialize response");
            transport
                .dispatch(&envelope.reply_to.token, &response_payload)
                .expect("dispatch should succeed");

            let result = handle.await.expect("task should complete");
            assert_eq!(result.expect("should succeed"), TestResponse { value: 99 });
        });
    }

    #[test]
    fn test_send_request_raw_future() {
        let transport = create_test_transport();

        let server_token = UID::new(0x1234, 0x5678);
        let server_endpoint = Endpoint::new(test_address(), server_token);

        let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<TestRequest>, JsonCodec>> =
            Rc::new(NetNotifiedQueue::new(server_endpoint.clone(), JsonCodec));
        transport.register(server_token, server_queue.clone());

        let ep: ServiceEndpoint<TestRequest, TestResponse, JsonCodec> =
            ServiceEndpoint::new(server_endpoint, JsonCodec);

        let _future = ep
            .send_request(&transport, TestRequest { value: 7 })
            .expect("send_request should succeed");

        let envelope = server_queue.try_recv().expect("should receive envelope");
        assert_eq!(envelope.request, TestRequest { value: 7 });
    }
}
