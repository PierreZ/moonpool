//! Server-authenticated TLS over real TCP with actual certificates (feature
//! `tls`), on both Tokio flavors: server identity and name validation,
//! untrusted and expired certificates, certificate and trust rotation,
//! signed tokens over TLS and downgrade rejection.

#![cfg(all(feature = "prost", feature = "tls"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use moonpool_core::TokioProviders;
use moonpool_rpc::security::tls::{
    CertificateDer, PrivateKeyDer, ServerNames, Tls, TlsClient, TlsServer,
};
use moonpool_rpc::security::{
    AccessRequest, Credential, CredentialError, FixedUtc, Principal, RequestVerifier,
    SecurityConfig, UtcTime,
};
use moonpool_rpc::{
    AccessClass, ErrorReason, Execution, IncomingRequest, MethodId, RequestStream, RpcConfig,
    RpcDriver, RpcError, RpcHandle, RpcMethod, SchemaVersion, ServiceRef,
};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, SanType};
use time::{Date, Month, OffsetDateTime, Time};

#[derive(Clone, PartialEq, prost::Message)]
struct Text {
    #[prost(string, tag = "1")]
    text: String,
}

struct Echo;
impl RpcMethod for Echo {
    type Request = Text;
    type Reply = Text;
    const METHOD: MethodId = MethodId::new(0x7150);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}

const CALL: Duration = Duration::from_secs(10);
/// 2030-06-01T00:00:00Z: inside every test leaf's validity.
const JUNE_2030: u64 = 1_906_502_400;
/// 2032-01-01T00:00:00Z: past the leaves' `not_after`.
const Y2032: u64 = 1_956_528_000;

fn at(year: i32) -> OffsetDateTime {
    OffsetDateTime::new_utc(
        Date::from_calendar_date(year, Month::January, 1).expect("date"),
        Time::MIDNIGHT,
    )
}

/// A test CA (with its own distinct name) and leaves it signs.
struct Authority {
    root: CertificateDer<'static>,
    issuer: Issuer<'static, KeyPair>,
}

impl Authority {
    fn new(name: &str) -> Self {
        let key = KeyPair::generate().expect("key");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, name);
        params.not_before = at(2029);
        params.not_after = at(2040);
        let root = params.self_signed(&key).expect("root");
        Self {
            root: root.der().clone(),
            issuer: Issuer::new(params, key),
        }
    }

    /// A leaf for `127.0.0.1` (IP SAN) and `rpc.test`, valid 2030-2031.
    fn leaf(&self) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let key = KeyPair::generate().expect("key");
        let mut params = CertificateParams::new(vec!["rpc.test".to_string()]).expect("params");
        params
            .subject_alt_names
            .push(SanType::IpAddress("127.0.0.1".parse().expect("ip")));
        params.not_before = at(2030);
        params.not_after = at(2031);
        let leaf = params.signed_by(&key, &self.issuer).expect("leaf");
        (
            vec![leaf.der().clone()],
            PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
    }
}

type Driver = tokio::task::JoinHandle<std::io::Error>;

async fn tls_server(
    server: TlsServer,
    security: SecurityConfig,
) -> (RpcHandle<TokioProviders>, Driver) {
    let config = RpcConfig {
        security,
        ..RpcConfig::default()
    };
    let (driver, rpc) = RpcDriver::listen_with(
        TokioProviders::new(),
        "127.0.0.1:0",
        config,
        Tls::server(server),
    )
    .await
    .expect("bind");
    (rpc, tokio::spawn(driver.run()))
}

fn tls_client(client: TlsClient, config: RpcConfig) -> (RpcHandle<TokioProviders>, Driver) {
    let (driver, rpc) =
        RpcDriver::client_only_with(TokioProviders::new(), config, Tls::client(client))
            .expect("config");
    (rpc, tokio::spawn(driver.run()))
}

fn serve(
    mut stream: RequestStream<Echo>,
    executed: &Arc<AtomicU32>,
) -> tokio::task::JoinHandle<()> {
    let executed = Arc::clone(executed);
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            let encrypted = reply
                .peer()
                .is_some_and(moonpool_rpc::PeerContext::is_encrypted);
            let anonymous_peer = reply.peer().is_some_and(|peer| peer.identity().is_none());
            assert!(encrypted && anonymous_peer, "TLS, no client certificate");
            executed.fetch_add(1, Ordering::SeqCst);
            let _ = reply.send(&request);
        }
    })
}

async fn call(
    rpc: &RpcHandle<TokioProviders>,
    target: &ServiceRef<Echo>,
) -> Result<Text, RpcError> {
    target
        .bind(rpc)
        .try_get_reply_within(&Text { text: "hi".into() }, CALL)
        .await
}

fn clock(now: u64) -> FixedUtc {
    FixedUtc::new(UtcTime::from_unix_seconds(now))
}

fn connect_failed(result: &Result<Text, RpcError>) -> bool {
    matches!(result, Err(error)
        if matches!(error.reason(), ErrorReason::ConnectFailed(_))
            && error.execution() == Execution::NotAdmitted)
}

/// The client authenticates the server: its chain, its name and its
/// validity at the injected time. Every failure is a refused connect,
/// never an admitted request.
async fn the_server_identity_is_verified() {
    let authority = Authority::new("moonpool test ca");
    let (chain, key) = authority.leaf();
    let server_tls = TlsServer::new(chain, key, clock(JUNE_2030)).expect("server");
    let (server, server_driver) = tls_server(server_tls, SecurityConfig::trusted_network()).await;
    let executed = Arc::new(AtomicU32::new(0));
    let (service, stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let handler = serve(stream, &executed);

    // Dialed by IP: the IP SAN proves it.
    let good = TlsClient::new([authority.root.clone()], clock(JUNE_2030)).expect("client");
    let (client, client_driver) = tls_client(good, RpcConfig::default());
    assert_eq!(call(&client, &service).await.expect("verified").text, "hi");
    client_driver.abort();

    // The DNS name in the certificate, expected explicitly.
    let named = TlsClient::new([authority.root.clone()], clock(JUNE_2030))
        .expect("client")
        .with_server_names(ServerNames::Fixed("rpc.test".into()));
    let (client, client_driver) = tls_client(named, RpcConfig::default());
    assert!(call(&client, &service).await.is_ok());
    client_driver.abort();

    // Another name: the certificate does not prove it.
    let wrong_name = TlsClient::new([authority.root.clone()], clock(JUNE_2030))
        .expect("client")
        .with_server_names(ServerNames::Fixed("other.test".into()));
    let (client, client_driver) = tls_client(wrong_name, RpcConfig::default());
    assert!(connect_failed(&call(&client, &service).await));
    client_driver.abort();

    // Another authority: untrusted.
    let stranger = Authority::new("some other ca");
    let untrusted = TlsClient::new([stranger.root.clone()], clock(JUNE_2030)).expect("client");
    let (client, client_driver) = tls_client(untrusted, RpcConfig::default());
    assert!(connect_failed(&call(&client, &service).await));
    client_driver.abort();

    // The client's UTC is past the certificate's validity: expired.
    let late = TlsClient::new([authority.root.clone()], clock(Y2032)).expect("client");
    let (client, client_driver) = tls_client(late, RpcConfig::default());
    assert!(connect_failed(&call(&client, &service).await));
    client_driver.abort();

    // A plaintext client cannot talk to a TLS server either.
    let (plain_driver, plain) =
        RpcDriver::client_only(TokioProviders::new(), RpcConfig::default()).expect("config");
    let plain_driver = tokio::spawn(plain_driver.run());
    assert!(call(&plain, &service).await.is_err());
    plain_driver.abort();

    assert_eq!(
        executed.load(Ordering::SeqCst),
        2,
        "only verified sessions ran"
    );
    server_driver.abort();
    handler.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn the_server_identity_is_verified_current_thread() {
    the_server_identity_is_verified().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_identity_is_verified_multi_thread() {
    the_server_identity_is_verified().await;
}

/// The server's certificate rotates for new handshakes; a client rotates
/// its trust anchors for new connections.
async fn certificates_and_trust_rotate() {
    let first = Authority::new("first ca");
    let second = Authority::new("second ca");
    let (chain, key) = first.leaf();
    let server_tls = TlsServer::new(chain, key, clock(JUNE_2030)).expect("server");
    let certificate = Arc::clone(server_tls.certificate());
    let (server, server_driver) = tls_server(server_tls, SecurityConfig::trusted_network()).await;
    let executed = Arc::new(AtomicU32::new(0));
    let (service, stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let handler = serve(stream, &executed);

    let trusting_first = TlsClient::new([first.root.clone()], clock(JUNE_2030)).expect("client");
    let (client, client_driver) = tls_client(trusting_first.clone(), RpcConfig::default());
    assert!(call(&client, &service).await.is_ok());
    client_driver.abort();

    let (chain, key) = second.leaf();
    certificate.replace(chain, key).expect("rotate");
    let (client, client_driver) = tls_client(trusting_first.clone(), RpcConfig::default());
    assert!(
        connect_failed(&call(&client, &service).await),
        "the new certificate is not trusted yet"
    );
    client_driver.abort();

    trusting_first
        .replace_roots([second.root.clone()])
        .expect("new roots");
    let (client, client_driver) = tls_client(trusting_first, RpcConfig::default());
    assert!(call(&client, &service).await.is_ok());
    client_driver.abort();

    // A certificate whose key does not match is refused; the current one
    // stays.
    let (chain, _) = second.leaf();
    let (_, other_key) = second.leaf();
    assert!(certificate.replace(chain, other_key).is_err());
    assert_eq!(executed.load(Ordering::SeqCst), 2);
    server_driver.abort();
    handler.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn certificates_and_trust_rotate_current_thread() {
    certificates_and_trust_rotate().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certificates_and_trust_rotate_multi_thread() {
    certificates_and_trust_rotate().await;
}

struct OneToken;
impl RequestVerifier for OneToken {
    fn verify(
        &self,
        _request: &AccessRequest<'_>,
        credential: &[u8],
    ) -> Result<Principal, CredentialError> {
        if credential == b"right" {
            Ok(Principal::new("client"))
        } else {
            Err(CredentialError::BadSignature)
        }
    }
}

/// Over TLS a bearer credential is accepted without the plaintext opt-in;
/// wrong credentials are refused; a version 1 client is refused at the
/// handshake (no downgrade to an unauthenticated session).
async fn credentials_over_tls_and_downgrade_rejection() {
    let authority = Authority::new("token ca");
    let (chain, key) = authority.leaf();
    let server_tls = TlsServer::new(chain, key, clock(JUNE_2030)).expect("server");
    let (server, server_driver) = tls_server(server_tls, SecurityConfig::enforced(OneToken)).await;
    let executed = Arc::new(AtomicU32::new(0));
    let (service, stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let handler = serve(stream, &executed);
    let tls = TlsClient::new([authority.root.clone()], clock(JUNE_2030)).expect("client");
    let with = |credential: &str, versions| RpcConfig {
        security: SecurityConfig::default().with_credentials(Credential::bearer(credential)),
        protocol_versions: versions,
        ..RpcConfig::default()
    };

    let (client, client_driver) = tls_client(tls.clone(), with("right", 1..=2));
    assert!(call(&client, &service).await.is_ok());
    let wrong = service
        .bind(&client)
        .with_credentials(Credential::bearer("wrong"))
        .try_get_reply_within(&Text { text: "x".into() }, CALL)
        .await
        .expect_err("wrong credential");
    assert_eq!(
        wrong.reason(),
        &ErrorReason::Unauthenticated(CredentialError::BadSignature)
    );
    assert_eq!(wrong.execution(), Execution::NotAdmitted);
    client_driver.abort();

    let (old, old_driver) = tls_client(tls, with("right", 1..=1));
    assert!(connect_failed(&call(&old, &service).await));
    old_driver.abort();

    assert_eq!(executed.load(Ordering::SeqCst), 1);
    assert!(server.stats().expect("running").version_rejections >= 1);
    server_driver.abort();
    handler.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn credentials_over_tls_and_downgrade_rejection_current_thread() {
    credentials_over_tls_and_downgrade_rejection().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credentials_over_tls_and_downgrade_rejection_multi_thread() {
    credentials_over_tls_and_downgrade_rejection().await;
}

/// A graceful shutdown closes TLS sessions in order: the client sees the
/// session end (its call fails as a disconnect), not a protocol error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_graceful_shutdown_closes_tls_sessions_in_order() {
    let authority = Authority::new("shutdown ca");
    let (chain, key) = authority.leaf();
    let server_tls = TlsServer::new(chain, key, clock(JUNE_2030)).expect("server");
    let (server, server_driver) = tls_server(server_tls, SecurityConfig::trusted_network()).await;
    let (service, mut stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let holder = tokio::spawn(async move {
        // Hold every request: nothing drains.
        let mut held = Vec::new();
        while let Some(request) = stream.recv().await {
            held.push(request);
        }
    });
    let tls = TlsClient::new([authority.root.clone()], clock(JUNE_2030)).expect("client");
    let (client, client_driver) = tls_client(tls, RpcConfig::default());
    let pending = {
        let service = service.clone();
        let client = client.clone();
        tokio::spawn(async move { call(&client, &service).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let report = server.shutdown(Duration::from_millis(100)).await;
    assert!(!report.drained);
    assert_eq!(report.replies_abandoned, 1);
    assert!(report.closed_cleanly, "{report:?}");
    let outcome = pending.await.expect("join").expect_err("abandoned");
    assert_eq!(outcome.reason(), &ErrorReason::Disconnected);
    assert_eq!(outcome.execution(), Execution::MaybeExecuted);
    let stats = client.stats().expect("running");
    assert_eq!(stats.protocol_violations, 0, "an orderly TLS close");
    client_driver.abort();
    server_driver.abort();
    holder.abort();
}
