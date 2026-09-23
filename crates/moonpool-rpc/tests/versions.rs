//! Protocol versions: golden cross-version fixtures, version-aware
//! decoding, and old/new runtimes talking over real TCP (both Tokio
//! flavors).
//!
//! A runtime restricted to version 1 (`RpcConfig::protocol_versions =
//! 1..=1`) is a faithful old peer: version 1 frames are byte for byte what
//! the version 1 build sent (the fixtures pin them), and a version 1
//! session decodes only version 1 statuses, as the old build did.

#![cfg(feature = "prost")]

use std::time::Duration;

use moonpool_core::TokioProviders;
use moonpool_rpc::protocol::metadata::encode_bearer;
use moonpool_rpc::protocol::{
    EnvelopeError, PROTOCOL_MAGIC, REQUEST_FLAG_ONE_WAY, REQUEST_FLAG_STREAM, WireError,
    WireMessage, WireOutcome, decode_message_at, encode_frame, encode_message,
};
use moonpool_rpc::security::{
    AccessRequest, Credential, CredentialError, Principal, RequestVerifier, SecurityConfig,
};
use moonpool_rpc::{
    AccessClass, CodecId, EndpointToken, ErrorReason, Execution, Incarnation, IncomingRequest,
    InterfaceId, MethodId, RequestStream, RpcConfig, RpcDriver, RpcHandle, RpcMethod,
    SchemaVersion,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn fixture(file: &str, name: &str) -> Vec<u8> {
    let hex = file
        .lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
        .unwrap_or_else(|| panic!("fixture {name} missing"));
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex"))
        .collect()
}

fn names(file: &str) -> Vec<&str> {
    file.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .filter_map(|line| line.split(' ').next())
        .collect()
}

const V1: &str = include_str!("fixtures/wire-v1.txt");
const V2: &str = include_str!("fixtures/wire-v2.txt");

fn request(metadata: Vec<u8>, flags: u8, stream_window: u64) -> WireMessage {
    WireMessage::Request {
        call_id: 7,
        incarnation: Incarnation::from_raw(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10),
        token: EndpointToken::from_parts(3, 1),
        interface: InterfaceId::new(0x6b76_0001),
        interface_version: SchemaVersion::new(2),
        method: MethodId::new(0x20),
        schema: SchemaVersion::new(1),
        codec: CodecId::PROST,
        flags,
        stream_window,
        metadata,
        body: vec![0x0a, 0x02, b'h', b'i'],
    }
}

fn hello(min_version: u16, max_version: u16, listen: Option<&str>) -> WireMessage {
    WireMessage::Hello {
        magic: PROTOCOL_MAGIC,
        min_version,
        max_version,
        incarnation: Incarnation::from_raw(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10),
        features: 0,
        max_frame_bytes: 1 << 20,
        listen: listen.map(|address| address.parse().expect("address")),
    }
}

fn reply(error: WireError) -> WireMessage {
    WireMessage::Reply {
        call_id: 7,
        outcome: WireOutcome::Err(error),
    }
}

/// Every version 1 message layout, as named in `wire-v1.txt`.
fn v1_messages() -> Vec<(&'static str, WireMessage)> {
    let mut messages = v1_sessions_and_requests();
    messages.extend(v1_replies());
    messages.extend(v1_streams());
    messages
}

fn v1_sessions_and_requests() -> Vec<(&'static str, WireMessage)> {
    vec![
        ("hello-v1-only", hello(1, 1, None)),
        ("hello-v1-v2-listen-v4", hello(1, 2, Some("10.0.1.1:4500"))),
        (
            "hello-v1-v2-listen-v6",
            hello(1, 2, Some("[2001:db8::1]:4500")),
        ),
        ("request", request(Vec::new(), 0, 0)),
        (
            "request-one-way",
            request(Vec::new(), REQUEST_FLAG_ONE_WAY, 0),
        ),
        (
            "request-stream",
            request(Vec::new(), REQUEST_FLAG_STREAM, 0x0001_0000),
        ),
        (
            "reply-ok",
            WireMessage::Reply {
                call_id: 7,
                outcome: WireOutcome::Ok {
                    codec: CodecId::PROST,
                    body: vec![0x0a, 0x02, b'h', b'i'],
                },
            },
        ),
        ("ping", WireMessage::Ping { nonce: 9 }),
        ("pong", WireMessage::Pong { nonce: 9 }),
    ]
}

fn v1_replies() -> Vec<(&'static str, WireMessage)> {
    vec![
        (
            "reply-endpoint-not-found",
            reply(WireError::EndpointNotFound),
        ),
        (
            "reply-stale-incarnation",
            reply(WireError::StaleIncarnation),
        ),
        (
            "reply-method-mismatch",
            reply(WireError::MethodMismatch {
                registered: MethodId::new(0x21),
            }),
        ),
        (
            "reply-schema-mismatch",
            reply(WireError::SchemaMismatch {
                registered: SchemaVersion::new(2),
            }),
        ),
        (
            "reply-codec-mismatch",
            reply(WireError::CodecMismatch {
                registered: CodecId::new(0x8001),
            }),
        ),
        (
            "reply-malformed-request",
            reply(WireError::MalformedRequest),
        ),
        ("reply-overloaded", reply(WireError::Overloaded)),
        ("reply-broken-promise", reply(WireError::BrokenPromise)),
        ("reply-too-large", reply(WireError::ReplyTooLarge)),
        ("reply-encode-failed", reply(WireError::ReplyEncodeFailed)),
        ("reply-method-not-found", reply(WireError::MethodNotFound)),
        (
            "reply-interface-mismatch",
            reply(WireError::InterfaceMismatch {
                registered: (InterfaceId::new(0x6b76_0001), SchemaVersion::new(1)),
            }),
        ),
        (
            "reply-stream-failed",
            reply(WireError::StreamFailed { code: 42 }),
        ),
        ("reply-stream-protocol", reply(WireError::StreamProtocol)),
        (
            "reply-streaming-mismatch",
            reply(WireError::StreamingMismatch {
                endpoint_streams: true,
            }),
        ),
    ]
}

fn v1_streams() -> Vec<(&'static str, WireMessage)> {
    vec![
        (
            "stream-item",
            WireMessage::StreamItem {
                call_id: 7,
                sequence: 3,
                codec: CodecId::PROST,
                body: vec![0x08, 0x01],
            },
        ),
        (
            "stream-end",
            WireMessage::StreamEnd {
                call_id: 7,
                items: 4,
                error: None,
            },
        ),
        (
            "stream-end-failed",
            WireMessage::StreamEnd {
                call_id: 7,
                items: 1,
                error: Some(WireError::StreamFailed { code: 5 }),
            },
        ),
        (
            "stream-ack",
            WireMessage::StreamAck {
                call_id: 7,
                consumed: 128,
            },
        ),
        ("stream-cancel", WireMessage::StreamCancel { call_id: 7 }),
    ]
}

/// What version 2 adds, as named in `wire-v2.txt`.
fn v2_messages() -> Vec<(&'static str, WireMessage)> {
    vec![
        ("hello-v2-only", hello(2, 2, None)),
        (
            "request-bearer",
            request(encode_bearer(b"eyJ.token.sig").expect("fits"), 0, 0),
        ),
        (
            "reply-unauthenticated-expired",
            reply(WireError::Unauthenticated {
                reason: CredentialError::Expired,
            }),
        ),
        (
            "reply-unauthenticated-unknown-key",
            reply(WireError::Unauthenticated {
                reason: CredentialError::UnknownKey,
            }),
        ),
        (
            "reply-permission-denied",
            reply(WireError::PermissionDenied),
        ),
        ("reply-shutting-down", reply(WireError::ShuttingDown)),
    ]
}

/// Version 1 frames are exactly what the fixtures say, decode at both
/// versions, and re-encode to the same bytes: no silent change, no loss.
#[test]
fn version_one_frames_are_pinned_and_decode_at_every_version() {
    let messages = v1_messages();
    assert_eq!(
        names(V1),
        messages.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        "every fixture is checked, and only those"
    );
    for (name, message) in messages {
        let bytes = fixture(V1, name);
        assert_eq!(encode_message(&message), bytes, "{name} encoding changed");
        for version in [1, 2] {
            assert_eq!(
                decode_message_at(&bytes, version).as_ref(),
                Ok(&message),
                "{name} at version {version}"
            );
        }
    }
}

/// Version 2 additions decode at version 2 only; a version 1 session sees
/// an unknown status (the connection closes), never a guessed meaning.
#[test]
fn version_two_additions_are_refused_by_version_one() {
    let messages = v2_messages();
    assert_eq!(
        names(V2),
        messages.iter().map(|(name, _)| *name).collect::<Vec<_>>()
    );
    for (name, message) in messages {
        let bytes = fixture(V2, name);
        assert_eq!(encode_message(&message), bytes, "{name} encoding changed");
        assert_eq!(
            decode_message_at(&bytes, 2).as_ref(),
            Ok(&message),
            "{name}"
        );
        match &message {
            WireMessage::Reply {
                outcome: WireOutcome::Err(error),
                ..
            } => {
                assert!(
                    matches!(
                        decode_message_at(&bytes, 1),
                        Err(EnvelopeError::UnknownStatus(16..=18))
                    ),
                    "{name} must not decode at version 1"
                );
                // What a version 2 server sends a version 1 peer instead.
                let downgraded = error.for_version(1);
                assert_eq!(downgraded.since(), 1);
                assert!(decode_message_at(&encode_message(&reply(downgraded)), 1).is_ok());
            }
            // The layout of a request or a hello never changed: a version 1
            // decoder reads the credential section as opaque metadata.
            _ => assert_eq!(decode_message_at(&bytes, 1).as_ref(), Ok(&message)),
        }
    }
}

/// Malformed frames and envelopes fail observably, at both versions.
#[test]
fn malformed_envelopes_and_frames_are_refused() {
    let request = fixture(V1, "request");
    let cases: Vec<(Vec<u8>, EnvelopeError)> = vec![
        (Vec::new(), EnvelopeError::Empty),
        (vec![0x7f], EnvelopeError::UnknownKind(0x7f)),
        (request[..20].to_vec(), EnvelopeError::Truncated),
        (
            {
                let mut ping = fixture(V1, "ping");
                ping.push(0);
                ping
            },
            EnvelopeError::TrailingBytes,
        ),
        (
            {
                let mut unknown = fixture(V1, "reply-overloaded");
                unknown[9] = 19;
                unknown
            },
            EnvelopeError::UnknownStatus(19),
        ),
    ];
    for (bytes, expected) in cases {
        for version in [1, 2] {
            assert_eq!(decode_message_at(&bytes, version), Err(expected.clone()));
        }
    }
    // Frame limits: a frame larger than the limit is refused before it is
    // buffered, a corrupt checksum before it is parsed.
    let mut decoder = moonpool_rpc::protocol::FrameDecoder::new(32);
    decoder.feed(&encode_frame(&request, u32::MAX).expect("frame"));
    assert!(decoder.next_frame().is_err(), "above the 32-byte limit");
    let mut corrupt = encode_frame(&request, u32::MAX).expect("frame");
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    let mut decoder = moonpool_rpc::protocol::FrameDecoder::new(1 << 20);
    decoder.feed(&corrupt);
    assert!(decoder.next_frame().is_err(), "a flipped bit");
    assert!(encode_frame(&request, 16).is_err(), "encoder bound");
}

// ---------------------------------------------------------------------------
// Old and new runtimes on real TCP.

#[derive(Clone, PartialEq, prost::Message)]
struct Text {
    #[prost(string, tag = "1")]
    text: String,
}

struct Echo;
impl RpcMethod for Echo {
    type Request = Text;
    type Reply = Text;
    const METHOD: MethodId = MethodId::new(0x7e57);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}

/// Accepts the credential `valid` as principal `tester`.
struct OneToken;
impl RequestVerifier for OneToken {
    fn verify(
        &self,
        _request: &AccessRequest<'_>,
        credential: &[u8],
    ) -> Result<Principal, CredentialError> {
        if credential == b"valid" {
            Ok(Principal::new("tester"))
        } else {
            Err(CredentialError::BadSignature)
        }
    }
}

type Driver = tokio::task::JoinHandle<std::io::Error>;

async fn server(config: RpcConfig) -> (RpcHandle<TokioProviders>, Driver) {
    let (driver, rpc) = RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", config)
        .await
        .expect("bind");
    (rpc, tokio::spawn(driver.run()))
}

fn client(config: RpcConfig) -> (RpcHandle<TokioProviders>, Driver) {
    let (driver, rpc) = RpcDriver::client_only(TokioProviders::new(), config).expect("config");
    (rpc, tokio::spawn(driver.run()))
}

fn serve(mut stream: RequestStream<Echo>) -> tokio::task::JoinHandle<u32> {
    tokio::spawn(async move {
        let mut executed = 0;
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            executed += 1;
            let _ = reply.send(&request);
        }
        executed
    })
}

fn v1_only() -> RpcConfig {
    RpcConfig {
        protocol_versions: 1..=1,
        ..RpcConfig::default()
    }
}

fn text(text: &str) -> Text {
    Text { text: text.into() }
}

const CALL: Duration = Duration::from_secs(5);

/// An old (version 1) client and a new server: the session runs version 1,
/// public calls work, and a refusal the old client cannot decode arrives
/// as the version 1 status that proves the same (never admitted), not as a
/// broken session.
async fn old_client_new_server() {
    let (server, server_driver) = server(RpcConfig::default()).await;
    let (public, public_stream) = server.register::<Echo>(AccessClass::Public).expect("reg");
    let (private, private_stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let public_served = serve(public_stream);
    let private_served = serve(private_stream);
    let (old, old_driver) = client(v1_only());
    let reply = public
        .bind(&old)
        .try_get_reply_within(&text("hi"), CALL)
        .await
        .expect("a public call across versions");
    assert_eq!(reply.text, "hi");
    let denied = private
        .bind(&old)
        .try_get_reply_within(&text("no"), CALL)
        .await
        .expect_err("private fails closed");
    assert_eq!(denied.reason(), &ErrorReason::EndpointNotFound);
    assert_eq!(denied.execution(), Execution::NotAdmitted);
    let stats = old.stats().expect("running");
    assert_eq!(stats.protocol_violations, 0, "no undecodable status sent");
    assert_eq!(stats.version_rejections, 0);
    let server_stats = server.stats().expect("running");
    assert_eq!(server_stats.requests_unauthenticated, 1);
    drop((public, private));
    old_driver.abort();
    server_driver.abort();
    drop(server);
    assert_eq!(public_served.await.expect("join"), 1);
    assert_eq!(private_served.await.expect("join"), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn old_client_new_server_current_thread() {
    old_client_new_server().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_client_new_server_multi_thread() {
    old_client_new_server().await;
}

/// A new client and an old trusted server: version 1. A request carrying a
/// credential is never written on a version 1 session (which could not
/// carry it); an anonymous one works.
async fn new_client_old_server() {
    let (server, server_driver) = server(RpcConfig {
        security: SecurityConfig::trusted_network(),
        ..v1_only()
    })
    .await;
    let (service, stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let handler = serve(stream);
    let (new, new_driver) = client(RpcConfig {
        security: SecurityConfig::default().send_credentials_over_plaintext(),
        ..RpcConfig::default()
    });
    let withheld = service
        .bind(&new)
        .with_credentials(Credential::bearer("valid"))
        .try_get_reply_within(&text("hi"), CALL)
        .await
        .expect_err("no credential on a version 1 session");
    assert!(
        matches!(withheld.reason(), ErrorReason::CredentialWithheld(_)),
        "{withheld}"
    );
    assert_eq!(withheld.execution(), Execution::NotAdmitted);
    let reply = service
        .bind(&new)
        .try_get_reply_within(&text("hi"), CALL)
        .await
        .expect("anonymous call");
    assert_eq!(reply.text, "hi");
    assert_eq!(server.stats().expect("running").requests_authenticated, 0);
    drop(service);
    new_driver.abort();
    server_driver.abort();
    drop(server);
    assert_eq!(handler.await.expect("join"), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn new_client_old_server_current_thread() {
    new_client_old_server().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_client_old_server_multi_thread() {
    new_client_old_server().await;
}

/// Downgrade rejection: a server that verifies credentials speaks version 2
/// only, so a version 1 client (which could not carry a credential) is
/// refused at the handshake, observably, instead of being admitted
/// anonymously. A version 2 client with a valid credential gets through.
async fn a_verifying_server_refuses_version_one_clients() {
    let security = SecurityConfig::enforced(OneToken).accept_credentials_over_plaintext();
    let config = RpcConfig {
        security,
        ..RpcConfig::default()
    };
    assert_eq!(config.advertised_versions(), (2, 2));
    let (server, server_driver) = server(config).await;
    let (service, stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let handler = serve(stream);
    let (old, old_driver) = client(RpcConfig {
        security: SecurityConfig::default().with_credentials(Credential::bearer("valid")),
        ..v1_only()
    });
    let refused = service
        .bind(&old)
        .try_get_reply_within(&text("old"), CALL)
        .await
        .expect_err("no common version");
    assert!(
        matches!(refused.reason(), ErrorReason::ConnectFailed(_)),
        "{refused}"
    );
    assert_eq!(refused.execution(), Execution::NotAdmitted);
    let (new, new_driver) = client(RpcConfig {
        security: SecurityConfig::default()
            .with_credentials(Credential::bearer("valid"))
            .send_credentials_over_plaintext(),
        ..RpcConfig::default()
    });
    let reply = service
        .bind(&new)
        .try_get_reply_within(&text("new"), CALL)
        .await
        .expect("version 2 with a credential");
    assert_eq!(reply.text, "new");
    let stats = server.stats().expect("running");
    assert!(stats.version_rejections >= 1, "{stats:?}");
    assert_eq!(stats.requests_authenticated, 1);
    drop(service);
    old_driver.abort();
    new_driver.abort();
    server_driver.abort();
    drop(server);
    assert_eq!(
        handler.await.expect("join"),
        1,
        "only the version 2 call ran"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_verifying_server_refuses_version_one_clients_current_thread() {
    a_verifying_server_refuses_version_one_clients().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verifying_server_refuses_version_one_clients_multi_thread() {
    a_verifying_server_refuses_version_one_clients().await;
}

/// A peer announcing only versions this build does not know is refused
/// before anything else, and counted.
#[tokio::test(flavor = "current_thread")]
async fn unknown_versions_are_refused_at_the_handshake() {
    let (server, server_driver) = server(RpcConfig::default()).await;
    let (service, stream) = server.register::<Echo>(AccessClass::Public).expect("reg");
    let handler = serve(stream);
    let address = service.endpoint().address().to_string();
    let mut raw = tokio::net::TcpStream::connect(&address)
        .await
        .expect("connect");
    let hello = encode_frame(&encode_message(&hello(3, 4, None)), u32::MAX).expect("frame");
    raw.write_all(&hello).await.expect("write");
    let closed = tokio::time::timeout(CALL, async {
        let mut sink = [0; 512];
        loop {
            match raw.read(&mut sink).await {
                Ok(0) | Err(_) => return true,
                Ok(_) => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(closed, "the server closes a session with no common version");
    let stats = server.stats().expect("running");
    assert_eq!(stats.version_rejections, 1);
    assert_eq!(stats.requests_admitted, 0);
    drop(service);
    server_driver.abort();
    drop(server);
    assert_eq!(handler.await.expect("join"), 0);
}

/// Configurations outside the supported window are refused up front.
#[test]
fn version_ranges_are_validated() {
    let backwards = std::ops::RangeInclusive::new(2, 1);
    for range in [0..=2, 1..=3, backwards] {
        let config = RpcConfig {
            protocol_versions: range.clone(),
            ..RpcConfig::default()
        };
        assert!(config.validate().is_err(), "{range:?}");
    }
    let verifying_v1 = RpcConfig {
        protocol_versions: 1..=1,
        security: SecurityConfig::enforced(OneToken),
        ..RpcConfig::default()
    };
    assert!(
        verifying_v1.validate().is_err(),
        "verifying credentials needs version 2"
    );
    assert_eq!(RpcConfig::default().advertised_versions(), (1, 2));
}
