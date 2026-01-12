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
    Endpoint, JsonCodec, NetTransport, NetworkAddress, NetworkProvider, ReplyError, ReplyFuture,
    RequestEnvelope, RequestStream, TokioRandomProvider, TokioTaskProvider, TokioTimeProvider, UID,
    send_request,
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

fn test_address() -> NetworkAddress {
    NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500)
}

fn create_transport()
-> NetTransport<MockNetworkProvider, TokioTimeProvider, TokioTaskProvider, TokioRandomProvider> {
    NetTransport::new(
        test_address(),
        MockNetworkProvider,
        TokioTimeProvider::new(),
        TokioTaskProvider,
        TokioRandomProvider::new(),
    )
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
    let transport = Rc::new(create_transport());

    // Create server endpoint
    let server_token = UID::new(0x1234, 0x5678);
    let server_endpoint = Endpoint::new(test_address(), server_token);

    // Create request stream for server
    let request_stream: RequestStream<PingRequest, JsonCodec> =
        RequestStream::new(server_endpoint.clone(), JsonCodec);

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
        .try_recv(move |endpoint, payload| {
            // Send reply back via transport
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
    let transport = Rc::new(create_transport());

    let server_token = UID::new(0xAAAA, 0xBBBB);
    let server_endpoint = Endpoint::new(test_address(), server_token);

    let request_stream: RequestStream<EchoRequest, JsonCodec> =
        RequestStream::new(server_endpoint.clone(), JsonCodec);

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
            .try_recv(move |endpoint, payload| {
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
    let transport = Rc::new(create_transport());

    let server_token = UID::new(0xDEAD, 0xBEEF);
    let server_endpoint = Endpoint::new(test_address(), server_token);

    let request_stream: RequestStream<PingRequest, JsonCodec> =
        RequestStream::new(server_endpoint.clone(), JsonCodec);

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
            .try_recv::<_, PingResponse>(move |endpoint, payload| {
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
    let transport = Rc::new(create_transport());

    let server_token = UID::new(0x1111, 0x2222);
    let server_endpoint = Endpoint::new(test_address(), server_token);

    let request_stream: RequestStream<PingRequest, JsonCodec> =
        RequestStream::new(server_endpoint.clone(), JsonCodec);

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
        .try_recv::<_, PingResponse>(move |endpoint, payload| {
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
