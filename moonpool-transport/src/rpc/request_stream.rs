//! RequestStream: Server-side stream for receiving typed requests.
//!
//! A RequestStream receives incoming requests and provides `ReplyPromise` handles
//! for each request, allowing the server to respond.
//!
//! # FDB Reference
//! Based on `RequestStream<T>` from fdbrpc.h.
//!
//! # Example
//!
//! ```rust,ignore
//! // Server loop
//! let stream = RequestStream::<PingRequest, _, JsonCodec>::new(endpoint, codec);
//!
//! loop {
//!     if let Some((request, reply)) = stream.recv(&transport).await {
//!         // Handle request and send reply
//!         reply.send(PingResponse { pong: request.ping });
//!     }
//! }
//! ```

use std::rc::Rc;

use crate::{Endpoint, MessageCodec, Providers};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::net_notified_queue::NetNotifiedQueue;
use super::net_transport::NetTransport;
use super::reply_promise::ReplyPromise;

/// Envelope wrapping a request with its reply endpoint.
///
/// When a client sends a request, it includes the endpoint where it's
/// listening for the response. This envelope bundles both.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RequestEnvelope<T> {
    /// The actual request payload.
    pub request: T,

    /// Endpoint where the client is listening for the response.
    pub reply_to: Endpoint,
}

/// Stream for receiving typed requests with reply promises.
///
/// The server uses this to receive incoming requests. Each received request
/// comes with a `ReplyPromise` that must be fulfilled.
pub struct RequestStream<Req, C: MessageCodec> {
    /// Queue for incoming request envelopes.
    queue: Rc<NetNotifiedQueue<RequestEnvelope<Req>, C>>,

    /// Codec for serializing responses.
    codec: C,
}

impl<Req, C> RequestStream<Req, C>
where
    Req: DeserializeOwned + 'static,
    C: MessageCodec,
{
    /// Create a new request stream with the given endpoint and codec.
    pub fn new(endpoint: Endpoint, codec: C) -> Self {
        Self {
            queue: Rc::new(NetNotifiedQueue::new(endpoint, codec.clone())),
            codec,
        }
    }

    /// Get the endpoint for this stream.
    ///
    /// Clients use this endpoint to send requests.
    pub fn endpoint(&self) -> &Endpoint {
        self.queue.endpoint()
    }

    /// Get a reference to the internal queue for registration.
    ///
    /// This is used to register the stream with NetTransport.
    pub fn queue(&self) -> Rc<NetNotifiedQueue<RequestEnvelope<Req>, C>> {
        self.queue.clone()
    }

    /// Receive the next request with its reply promise.
    ///
    /// Returns `None` if the stream is closed.
    ///
    /// The `sender` function is used to send the reply back to the client.
    /// Typically this is provided by the NetTransport.
    pub async fn recv<F, Resp>(&self, sender: F) -> Option<(Req, ReplyPromise<Resp, C>)>
    where
        Resp: Serialize,
        F: FnOnce(&Endpoint, &[u8]) + 'static,
    {
        let envelope = self.queue.recv().await?;

        let reply = ReplyPromise::new(envelope.reply_to, self.codec.clone(), sender);

        Some((envelope.request, reply))
    }

    /// Try to receive a request without blocking.
    ///
    /// Returns `None` if no request is available.
    pub fn try_recv<F, Resp>(&self, sender: F) -> Option<(Req, ReplyPromise<Resp, C>)>
    where
        Resp: Serialize,
        F: FnOnce(&Endpoint, &[u8]) + 'static,
    {
        let envelope = self.queue.try_recv()?;

        let reply = ReplyPromise::new(envelope.reply_to, self.codec.clone(), sender);

        Some((envelope.request, reply))
    }

    /// Receive the next request with embedded transport for replies.
    ///
    /// This is the preferred method for typical RPC usage. It eliminates
    /// the need for a manual closure callback by using the transport directly.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Before (with closure):
    /// let transport_clone = transport.clone();
    /// let (req, reply) = stream.recv(move |ep, payload| {
    ///     let _ = transport_clone.send_reliable(ep, payload);
    /// }).await?;
    ///
    /// // After (cleaner):
    /// let (req, reply) = stream.recv_with_transport(&transport).await?;
    /// ```
    pub async fn recv_with_transport<P, Resp>(
        &self,
        transport: &Rc<NetTransport<P>>,
    ) -> Option<(Req, ReplyPromise<Resp, C>)>
    where
        P: Providers,
        Resp: Serialize,
    {
        let transport_clone = Rc::clone(transport);
        self.recv(move |endpoint, payload| {
            let _ = transport_clone.send_reliable(endpoint, payload);
        })
        .await
    }

    /// Try to receive a request without blocking, with embedded transport.
    ///
    /// Non-blocking version of [`recv_with_transport`](Self::recv_with_transport).
    pub fn try_recv_with_transport<P, Resp>(
        &self,
        transport: &Rc<NetTransport<P>>,
    ) -> Option<(Req, ReplyPromise<Resp, C>)>
    where
        P: Providers,
        Resp: Serialize,
    {
        let transport_clone = Rc::clone(transport);
        self.try_recv(move |endpoint, payload| {
            let _ = transport_clone.send_reliable(endpoint, payload);
        })
    }

    /// Check if the stream is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Get the number of pending requests.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Close the stream.
    pub fn close(&self) {
        self.queue.close();
    }

    /// Check if the stream is closed.
    pub fn is_closed(&self) -> bool {
        self.queue.is_closed()
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
    use crate::MessageReceiver;

    fn test_endpoint() -> Endpoint {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        Endpoint::new(addr, UID::new(1, 1))
    }

    fn reply_endpoint() -> Endpoint {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4501);
        Endpoint::new(addr, UID::new(2, 2))
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
    fn test_request_stream_endpoint() {
        let endpoint = test_endpoint();
        let stream: RequestStream<PingRequest, JsonCodec> =
            RequestStream::new(endpoint.clone(), JsonCodec);

        assert_eq!(stream.endpoint().token, endpoint.token);
    }

    #[test]
    fn test_request_envelope_serde() {
        let envelope = RequestEnvelope {
            request: PingRequest { seq: 42 },
            reply_to: reply_endpoint(),
        };

        let json = serde_json::to_string(&envelope).expect("serialize");
        let decoded: RequestEnvelope<PingRequest> =
            serde_json::from_str(&json).expect("deserialize");

        assert_eq!(decoded.request, envelope.request);
        assert_eq!(decoded.reply_to.token, envelope.reply_to.token);
    }

    #[test]
    fn test_request_stream_try_recv_empty() {
        let stream: RequestStream<PingRequest, JsonCodec> =
            RequestStream::new(test_endpoint(), JsonCodec);

        let result: Option<(PingRequest, ReplyPromise<PingResponse, JsonCodec>)> =
            stream.try_recv(|_, _| {});

        assert!(result.is_none());
        assert!(stream.is_empty());
    }

    #[test]
    fn test_request_stream_try_recv() {
        let stream: RequestStream<PingRequest, JsonCodec> =
            RequestStream::new(test_endpoint(), JsonCodec);

        // Simulate receiving a request
        let envelope = RequestEnvelope {
            request: PingRequest { seq: 123 },
            reply_to: reply_endpoint(),
        };
        let payload = serde_json::to_vec(&envelope).expect("serialize");
        stream.queue().receive(&payload);

        assert!(!stream.is_empty());
        assert_eq!(stream.len(), 1);

        // Receive the request
        let sent_data: Rc<RefCell<Option<Vec<u8>>>> = Rc::new(RefCell::new(None));
        let sent_clone = sent_data.clone();

        let result: Option<(PingRequest, ReplyPromise<PingResponse, JsonCodec>)> =
            stream.try_recv(move |_endpoint, payload| {
                *sent_clone.borrow_mut() = Some(payload.to_vec());
            });

        assert!(result.is_some());
        let (request, reply) = result.expect("should receive request");
        assert_eq!(request, PingRequest { seq: 123 });

        // Send a reply
        reply.send(PingResponse { seq: 123 });

        // Verify reply was sent
        assert!(sent_data.borrow().is_some());
    }

    #[tokio::test]
    async fn test_request_stream_recv_async() {
        let stream: RequestStream<PingRequest, JsonCodec> =
            RequestStream::new(test_endpoint(), JsonCodec);

        // Simulate receiving a request
        let envelope = RequestEnvelope {
            request: PingRequest { seq: 456 },
            reply_to: reply_endpoint(),
        };
        let payload = serde_json::to_vec(&envelope).expect("serialize");
        stream.queue().receive(&payload);

        // Async receive
        let result: Option<(PingRequest, ReplyPromise<PingResponse, JsonCodec>)> =
            stream.recv(|_, _| {}).await;

        assert!(result.is_some());
        let (request, _reply) = result.expect("should receive request");
        assert_eq!(request, PingRequest { seq: 456 });
    }

    #[tokio::test]
    async fn test_request_stream_closed() {
        let stream: RequestStream<PingRequest, JsonCodec> =
            RequestStream::new(test_endpoint(), JsonCodec);

        assert!(!stream.is_closed());
        stream.close();
        assert!(stream.is_closed());

        let result: Option<(PingRequest, ReplyPromise<PingResponse, JsonCodec>)> =
            stream.recv(|_, _| {}).await;

        assert!(result.is_none());
    }

    // =========================================================================
    // Phase 12C API Tests: recv_with_transport / try_recv_with_transport
    // =========================================================================

    use crate::{
        NetTransportBuilder, Providers, TokioRandomProvider, TokioTaskProvider, TokioTimeProvider,
    };

    // Simple mock network provider for testing
    #[derive(Clone)]
    struct MockNetworkProvider;

    struct DummyStream;

    impl tokio::io::AsyncRead for DummyStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy stream")))
        }
    }

    impl tokio::io::AsyncWrite for DummyStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy stream")))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy stream")))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy stream")))
        }
    }

    struct DummyListener;

    #[async_trait::async_trait(?Send)]
    impl crate::TcpListenerTrait for DummyListener {
        type TcpStream = DummyStream;

        async fn accept(&self) -> std::io::Result<(Self::TcpStream, String)> {
            Err(std::io::Error::other("dummy listener"))
        }

        fn local_addr(&self) -> std::io::Result<String> {
            Err(std::io::Error::other("dummy listener"))
        }
    }

    #[async_trait::async_trait(?Send)]
    impl crate::NetworkProvider for MockNetworkProvider {
        type TcpStream = DummyStream;
        type TcpListener = DummyListener;

        async fn bind(&self, _addr: &str) -> std::io::Result<Self::TcpListener> {
            Err(std::io::Error::other("mock bind"))
        }

        async fn connect(&self, _addr: &str) -> std::io::Result<Self::TcpStream> {
            Err(std::io::Error::other("mock connection"))
        }
    }

    /// Mock providers bundle for testing
    #[derive(Clone)]
    struct MockProviders {
        network: MockNetworkProvider,
        time: TokioTimeProvider,
        task: TokioTaskProvider,
        random: TokioRandomProvider,
    }

    impl MockProviders {
        fn new() -> Self {
            Self {
                network: MockNetworkProvider,
                time: TokioTimeProvider::new(),
                task: TokioTaskProvider,
                random: TokioRandomProvider::new(),
            }
        }
    }

    impl Providers for MockProviders {
        type Network = MockNetworkProvider;
        type Time = TokioTimeProvider;
        type Task = TokioTaskProvider;
        type Random = TokioRandomProvider;

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
    }

    fn create_test_transport() -> Rc<crate::NetTransport<MockProviders>> {
        NetTransportBuilder::new(MockProviders::new())
            .local_address(test_endpoint().address.clone())
            .build()
            .expect("build should succeed")
    }

    // Create a local reply endpoint (same address as transport for local delivery)
    fn local_reply_endpoint() -> Endpoint {
        let addr = test_endpoint().address;
        Endpoint::new(addr, UID::new(0xFF, 0xFF))
    }

    #[test]
    fn test_try_recv_with_transport_empty() {
        let transport = create_test_transport();

        // Register a handler using Phase 12C API
        let token = UID::new(0xABCD, 0x1234);
        let stream = transport.register_handler::<PingRequest, _>(token, JsonCodec);

        // Should return None when queue is empty
        let result: Option<(PingRequest, ReplyPromise<PingResponse, JsonCodec>)> =
            stream.try_recv_with_transport(&transport);

        assert!(result.is_none());
    }

    #[test]
    fn test_try_recv_with_transport() {
        let transport = create_test_transport();

        // Register a dummy handler for broken promise errors (local delivery)
        let reply_token = local_reply_endpoint().token;
        let reply_queue: Rc<
            crate::NetNotifiedQueue<Result<PingResponse, crate::ReplyError>, JsonCodec>,
        > = Rc::new(crate::NetNotifiedQueue::new(
            local_reply_endpoint(),
            JsonCodec,
        ));
        transport.register(
            reply_token,
            Rc::clone(&reply_queue) as Rc<dyn crate::MessageReceiver>,
        );

        // Register a handler using Phase 12C API
        let token = UID::new(0xABCD, 0x1234);
        let stream = transport.register_handler::<PingRequest, _>(token, JsonCodec);

        // Simulate receiving a request via the queue (with local reply endpoint)
        let envelope = RequestEnvelope {
            request: PingRequest { seq: 999 },
            reply_to: local_reply_endpoint(),
        };
        let payload = serde_json::to_vec(&envelope).expect("serialize");
        stream.queue().receive(&payload);

        // Receive using Phase 12C API
        let result: Option<(PingRequest, ReplyPromise<PingResponse, JsonCodec>)> =
            stream.try_recv_with_transport(&transport);

        assert!(result.is_some());
        let (request, _reply) = result.expect("should receive request");
        assert_eq!(request.seq, 999);
        // Note: _reply will be dropped, sending BrokenPromise to reply_queue (local)
    }

    #[tokio::test]
    async fn test_recv_with_transport() {
        let transport = create_test_transport();

        // Register a dummy handler for broken promise errors (local delivery)
        let reply_token = local_reply_endpoint().token;
        let reply_queue: Rc<
            crate::NetNotifiedQueue<Result<PingResponse, crate::ReplyError>, JsonCodec>,
        > = Rc::new(crate::NetNotifiedQueue::new(
            local_reply_endpoint(),
            JsonCodec,
        ));
        transport.register(
            reply_token,
            Rc::clone(&reply_queue) as Rc<dyn crate::MessageReceiver>,
        );

        // Register a handler using Phase 12C API
        let token = UID::new(0xABCD, 0x1234);
        let stream = transport.register_handler::<PingRequest, _>(token, JsonCodec);

        // Simulate receiving a request via the queue (with local reply endpoint)
        let envelope = RequestEnvelope {
            request: PingRequest { seq: 888 },
            reply_to: local_reply_endpoint(),
        };
        let payload = serde_json::to_vec(&envelope).expect("serialize");
        stream.queue().receive(&payload);

        // Receive using Phase 12C API (async version)
        let result: Option<(PingRequest, ReplyPromise<PingResponse, JsonCodec>)> =
            stream.recv_with_transport(&transport).await;

        assert!(result.is_some());
        let (request, _reply) = result.expect("should receive request");
        assert_eq!(request.seq, 888);
        // Note: _reply will be dropped, sending BrokenPromise to reply_queue (local)
    }

    #[test]
    fn test_recv_with_transport_reply_sends() {
        let transport = create_test_transport();

        // Register a handler for the reply endpoint to track if response was sent
        let reply_token = local_reply_endpoint().token;
        let reply_queue: Rc<
            crate::NetNotifiedQueue<Result<PingResponse, crate::ReplyError>, JsonCodec>,
        > = Rc::new(crate::NetNotifiedQueue::new(
            local_reply_endpoint(),
            JsonCodec,
        ));
        transport.register(
            reply_token,
            Rc::clone(&reply_queue) as Rc<dyn crate::MessageReceiver>,
        );

        // Register request handler
        let token = UID::new(0xABCD, 0x1234);
        let stream = transport.register_handler::<PingRequest, _>(token, JsonCodec);

        // Simulate receiving a request
        let envelope = RequestEnvelope {
            request: PingRequest { seq: 777 },
            reply_to: local_reply_endpoint(),
        };
        let payload = serde_json::to_vec(&envelope).expect("serialize");
        stream.queue().receive(&payload);

        // Receive and send reply using Phase 12C API
        let result: Option<(PingRequest, ReplyPromise<PingResponse, JsonCodec>)> =
            stream.try_recv_with_transport(&transport);
        let (request, reply) = result.expect("should receive request");
        assert_eq!(request.seq, 777);

        // Send response via ReplyPromise
        reply.send(PingResponse { seq: 777 });

        // Verify response was dispatched to reply endpoint
        let received = reply_queue.try_recv();
        assert!(received.is_some());
        let response = received.expect("should receive response");
        assert_eq!(
            response.expect("response should be Ok"),
            PingResponse { seq: 777 }
        );
    }
}
