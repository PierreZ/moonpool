//! Integration tests for the request-response RPC system.
//!
//! These tests exercise the full RPC flow including:
//! - Client sending requests via send_request()
//! - Server receiving via RequestStream
//! - Server responding via ReplyPromise
//! - Client receiving via ReplyFuture

use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;

use moonpool_transport::{
    Endpoint, FailureMonitor, FailureStatus, JsonCodec, MessagingError, NetTransport,
    NetworkAddress, NetworkProvider, Providers, ReplyError, ReplyFuture, RequestEnvelope,
    RequestStream, TokioRandomProvider, TokioStorageProvider, TokioTaskProvider, TokioTimeProvider,
    UID, send_request,
};
use serde::{Deserialize, Serialize};

// Mock network provider for local-only testing
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
impl moonpool_transport::TcpListenerTrait for DummyListener {
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

fn create_transport() -> Rc<NetTransport<MockProviders>> {
    moonpool_transport::NetTransportBuilder::new(MockProviders::new())
        .local_address(test_address())
        .build()
        .expect("build transport")
}

// Test message types
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PingRequest {
    seq: u32,
    payload: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PingResponse {
    seq: u32,
    echo: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct EchoRequest {
    message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct EchoResponse {
    message: String,
    length: usize,
}

/// Test basic ping-pong RPC flow
#[tokio::test]
async fn test_basic_ping_pong() {
    let transport = create_transport();

    // Create server endpoint
    let server_token = UID::new(0x1234, 0x5678);
    let server_endpoint = Endpoint::new(test_address(), server_token);

    // Create request stream for server
    let request_stream: RequestStream<PingRequest, PingResponse, JsonCodec, MockProviders> =
        RequestStream::new(server_endpoint.clone(), JsonCodec, Rc::clone(&transport));

    // Register server queue with transport
    transport.register(server_token, request_stream.queue());

    // Client sends request
    let future: ReplyFuture<PingResponse, JsonCodec> = send_request(
        &transport,
        &server_endpoint,
        PingRequest {
            seq: 1,
            payload: "hello".to_string(),
        },
        JsonCodec,
    )
    .expect("send_request should succeed");

    // Server receives and responds
    let transport_clone = transport.clone();
    let (request, reply) = request_stream
        .try_recv_with_sender(move |endpoint, payload| {
            transport_clone
                .dispatch(&endpoint.token, payload)
                .expect("dispatch should succeed");
        })
        .expect("should receive request");

    assert_eq!(request.seq, 1);
    assert_eq!(request.payload, "hello");

    // Server sends response
    reply.send(PingResponse {
        seq: 1,
        echo: "hello".to_string(),
    });

    // Client receives response
    let response = future.await;
    assert_eq!(
        response,
        Ok(PingResponse {
            seq: 1,
            echo: "hello".to_string()
        })
    );
}

/// Test multiple sequential requests
#[tokio::test]
async fn test_multiple_requests() {
    let transport = create_transport();

    let server_token = UID::new(0xAAAA, 0xBBBB);
    let server_endpoint = Endpoint::new(test_address(), server_token);

    let request_stream: RequestStream<EchoRequest, EchoResponse, JsonCodec, MockProviders> =
        RequestStream::new(server_endpoint.clone(), JsonCodec, Rc::clone(&transport));

    transport.register(server_token, request_stream.queue());

    // Send multiple requests
    for i in 0..5 {
        let message = format!("message_{}", i);

        let future: ReplyFuture<EchoResponse, JsonCodec> = send_request(
            &transport,
            &server_endpoint,
            EchoRequest {
                message: message.clone(),
            },
            JsonCodec,
        )
        .expect("send_request should succeed");

        // Server handles request
        let transport_clone = transport.clone();
        let (request, reply) = request_stream
            .try_recv_with_sender(move |endpoint, payload| {
                transport_clone
                    .dispatch(&endpoint.token, payload)
                    .expect("dispatch should succeed");
            })
            .expect("should receive request");

        let len = request.message.len();
        reply.send(EchoResponse {
            message: request.message,
            length: len,
        });

        // Client gets response
        let response = future.await.expect("should receive response");
        assert_eq!(response.message, message);
        assert_eq!(response.length, message.len());
    }
}

/// Test broken promise detection
#[tokio::test]
async fn test_broken_promise() {
    let transport = create_transport();

    let server_token = UID::new(0xDEAD, 0xBEEF);
    let server_endpoint = Endpoint::new(test_address(), server_token);

    let request_stream: RequestStream<PingRequest, PingResponse, JsonCodec, MockProviders> =
        RequestStream::new(server_endpoint.clone(), JsonCodec, Rc::clone(&transport));

    transport.register(server_token, request_stream.queue());

    // Client sends request
    let future: ReplyFuture<PingResponse, JsonCodec> = send_request(
        &transport,
        &server_endpoint,
        PingRequest {
            seq: 999,
            payload: "test".to_string(),
        },
        JsonCodec,
    )
    .expect("send_request should succeed");

    // Server receives but drops the promise without responding
    {
        let transport_clone = transport.clone();
        let (_request, reply) = request_stream
            .try_recv_with_sender(move |endpoint, payload| {
                transport_clone
                    .dispatch(&endpoint.token, payload)
                    .expect("dispatch should succeed");
            })
            .expect("should receive request");

        // Drop reply without sending - this should trigger BrokenPromise
        drop(reply);
    }

    // Client should receive BrokenPromise error
    let response = future.await;
    assert_eq!(response, Err(ReplyError::BrokenPromise));
}

/// Test server sending explicit error
#[tokio::test]
async fn test_explicit_error_response() {
    let transport = create_transport();

    let server_token = UID::new(0x1111, 0x2222);
    let server_endpoint = Endpoint::new(test_address(), server_token);

    let request_stream: RequestStream<PingRequest, PingResponse, JsonCodec, MockProviders> =
        RequestStream::new(server_endpoint.clone(), JsonCodec, Rc::clone(&transport));

    transport.register(server_token, request_stream.queue());

    // Client sends request
    let future: ReplyFuture<PingResponse, JsonCodec> = send_request(
        &transport,
        &server_endpoint,
        PingRequest {
            seq: 42,
            payload: "error_test".to_string(),
        },
        JsonCodec,
    )
    .expect("send_request should succeed");

    // Server receives and sends error
    let transport_clone = transport.clone();
    let (_request, reply) = request_stream
        .try_recv_with_sender(move |endpoint, payload| {
            transport_clone
                .dispatch(&endpoint.token, payload)
                .expect("dispatch should succeed");
        })
        .expect("should receive request");

    reply.send_error(ReplyError::Timeout);

    // Client receives the error
    let response = future.await;
    assert_eq!(response, Err(ReplyError::Timeout));
}

/// Test request envelope serialization
#[test]
fn test_request_envelope_roundtrip() {
    let envelope = RequestEnvelope {
        request: PingRequest {
            seq: 123,
            payload: "test payload".to_string(),
        },
        reply_to: Endpoint::new(test_address(), UID::new(0x9999, 0x8888)),
    };

    let json = serde_json::to_string(&envelope).expect("serialize");
    let decoded: RequestEnvelope<PingRequest> = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(decoded.request.seq, 123);
    assert_eq!(decoded.request.payload, "test payload");
    assert_eq!(decoded.reply_to.token, envelope.reply_to.token);
}

/// Test that local delivery to non-existent endpoint returns EndpointNotFound synchronously.
#[test]
fn test_endpoint_not_found_local_delivery() {
    let transport = create_transport();

    // Token that nobody registered
    let missing_token = UID::new(0xDEAD, 0xBEEF);
    let missing_endpoint = Endpoint::new(test_address(), missing_token);

    // send_request goes through deliver_local (same address) → EndpointNotFound
    let result = send_request::<PingRequest, PingResponse, _, _>(
        &transport,
        &missing_endpoint,
        PingRequest {
            seq: 1,
            payload: "hello".to_string(),
        },
        JsonCodec,
    );

    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("expected EndpointNotFound error"),
    };
    match err {
        MessagingError::EndpointNotFound { token } => {
            assert_eq!(token, missing_token);
        }
        other => panic!("expected EndpointNotFound, got {:?}", other),
    }
}

/// Test that dispatching an endpoint-not-found notification correctly marks
/// the endpoint as permanently failed in the failure monitor.
///
/// Simulates what happens when the client-side peer receives the notification:
/// parse the 16-byte payload → reconstruct endpoint → call fm.endpoint_not_found().
#[test]
fn test_endpoint_not_found_notifies_failure_monitor() {
    let transport = create_transport();
    let fm = transport.failure_monitor();

    let server_addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 5000);
    let missing_token = UID::new(0xCAFE, 0xBABE);
    let endpoint = Endpoint::new(server_addr.clone(), missing_token);

    // Mark as available first so we can observe the transition
    fm.set_status(&server_addr.to_string(), FailureStatus::Available);
    assert_eq!(fm.state(&endpoint), FailureStatus::Available);

    // Simulate receiving the notification: construct endpoint and call endpoint_not_found
    fm.endpoint_not_found(&endpoint);

    // Endpoint should now be permanently failed
    assert_eq!(fm.state(&endpoint), FailureStatus::Failed);
}

/// Test that well-known tokens are NOT notified (prevents feedback loops).
#[test]
fn test_endpoint_not_found_skips_well_known_tokens() {
    let fm = Rc::new(FailureMonitor::new());

    let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500);
    let well_known_token = UID::well_known(42);
    let endpoint = Endpoint::new(addr, well_known_token);

    fm.set_status("10.0.0.1:4500", FailureStatus::Available);

    // endpoint_not_found should skip well-known tokens
    fm.endpoint_not_found(&endpoint);

    // Should still be available (not marked as failed)
    assert_eq!(fm.state(&endpoint), FailureStatus::Available);
}

/// Test the notification wire format: 16 bytes = two LE u64 encoding a UID.
#[test]
fn test_endpoint_not_found_wire_format_roundtrip() {
    let original = UID::new(0x1234_5678_9ABC_DEF0, 0xFEDC_BA98_7654_3210);

    // Encode
    let mut payload = [0u8; 16];
    payload[0..8].copy_from_slice(&original.first.to_le_bytes());
    payload[8..16].copy_from_slice(&original.second.to_le_bytes());

    // Decode
    let first = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let second = u64::from_le_bytes(payload[8..16].try_into().unwrap());
    let decoded = UID::new(first, second);

    assert_eq!(decoded, original);
}
