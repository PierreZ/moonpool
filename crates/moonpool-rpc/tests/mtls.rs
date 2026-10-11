//! Mutual TLS over real TCP with certificates from the `pki` module
//! (feature `tls-pki`), on both Tokio flavors: required and optional client
//! certificates, untrusted and expired ones, the peer identity reaching the
//! access policy, and client-root and client-certificate rotation.

#![cfg(all(feature = "prost", feature = "tls-pki"))]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use moonpool_core::TokioProviders;
use moonpool_rpc::security::tls::pki::{
    Authority, Issued, KeySeed, LeafSpec, SubjectName, Usage, Validity,
};
use moonpool_rpc::security::tls::{ClientAuth, Tls, TlsClient, TlsServer};
use moonpool_rpc::security::{FixedUtc, PEER_IDENTITY_ISSUER, SecurityConfig, UtcTime};
use moonpool_rpc::{
    AccessClass, IncomingRequest, MethodId, RequestStream, RpcConfig, RpcDriver, RpcError,
    RpcHandle, RpcMethod, SchemaVersion, ServiceRef,
};

#[derive(Clone, PartialEq, prost::Message)]
struct Text {
    #[prost(string, tag = "1")]
    text: String,
}

struct Echo;
impl RpcMethod for Echo {
    type Request = Text;
    type Reply = Text;
    const METHOD: MethodId = MethodId::new(0x7151);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}

const CALL: Duration = Duration::from_secs(10);
/// 2030-06-01T00:00:00Z.
const NOW: u64 = 1_906_502_400;
const DAY: u64 = 86_400;

fn at(seconds: u64) -> UtcTime {
    UtcTime::from_unix_seconds(seconds)
}

fn clock(now: u64) -> FixedUtc {
    FixedUtc::new(at(now))
}

fn authority(name: &str) -> Authority {
    Authority::new(
        name,
        Validity::starting(at(NOW - DAY), 365 * DAY),
        KeySeed::Random,
    )
    .expect("authority")
}

fn leaf(authority: &Authority, names: Vec<SubjectName>, usage: Usage, valid: Validity) -> Issued {
    authority
        .issue(&LeafSpec {
            names,
            validity: valid,
            usage,
            seed: KeySeed::Random,
        })
        .expect("leaf")
}

fn this_year() -> Validity {
    Validity::starting(at(NOW - DAY), 30 * DAY)
}

fn server_leaf(authority: &Authority) -> Issued {
    leaf(
        authority,
        vec![SubjectName::Ip("127.0.0.1".parse().expect("ip"))],
        Usage::Server,
        this_year(),
    )
}

fn client_leaf(authority: &Authority, name: &str) -> Issued {
    leaf(
        authority,
        vec![SubjectName::Uri(format!("test://{name}"))],
        Usage::Client,
        this_year(),
    )
}

type Seen = Arc<Mutex<Vec<(Option<String>, Option<String>)>>>;

struct Server {
    _rpc: RpcHandle<TokioProviders>,
    private: ServiceRef<Echo>,
    public: ServiceRef<Echo>,
    seen: Seen,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn serve(mut stream: RequestStream<Echo>, seen: &Seen) -> tokio::task::JoinHandle<()> {
    let seen = Arc::clone(seen);
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            let identity = reply
                .peer()
                .and_then(|peer| peer.identity().map(ToString::to_string));
            let principal = reply
                .principal()
                .map(|principal| format!("{}@{:?}", principal.subject(), principal.issuer()));
            seen.lock().expect("lock").push((identity, principal));
            let _ = reply.send(&request);
        }
    })
}

async fn server(tls: TlsServer, security: SecurityConfig) -> Server {
    let config = RpcConfig {
        security,
        ..RpcConfig::default()
    };
    let (driver, rpc) = RpcDriver::listen_with(
        TokioProviders::new(),
        "127.0.0.1:0",
        config,
        Tls::server(tls),
    )
    .await
    .expect("bind");
    let driver = tokio::spawn(async move {
        let _ = driver.run().await;
    });
    let seen = Seen::default();
    let (private, private_stream) = rpc.register::<Echo>(AccessClass::Private).expect("reg");
    let (public, public_stream) = rpc.register::<Echo>(AccessClass::Public).expect("reg");
    let tasks = vec![
        driver,
        serve(private_stream, &seen),
        serve(public_stream, &seen),
    ];
    Server {
        _rpc: rpc,
        private,
        public,
        seen,
        tasks,
    }
}

async fn call(client: TlsClient, target: &ServiceRef<Echo>) -> Result<Text, RpcError> {
    let (driver, rpc) = RpcDriver::client_only_with(
        TokioProviders::new(),
        RpcConfig::default(),
        Tls::client(client),
    )
    .expect("config");
    let driver = tokio::spawn(driver.run());
    let result = target
        .bind(&rpc)
        .try_get_reply_within(&Text { text: "hi".into() }, CALL)
        .await;
    driver.abort();
    result
}

fn trusting(authority: &Authority) -> TlsClient {
    TlsClient::new([authority.certificate().clone()], clock(NOW)).expect("client")
}

fn presenting(authority: &Authority, issued: &Issued) -> TlsClient {
    trusting(authority)
        .with_certificate(issued.chain.clone(), issued.key())
        .expect("client certificate")
}

fn tls_server(authority: &Authority, client_auth: ClientAuth) -> TlsServer {
    let issued = server_leaf(authority);
    TlsServer::new(issued.chain.clone(), issued.key(), clock(NOW))
        .expect("server")
        .with_client_auth(client_auth)
        .expect("client auth")
}

/// A server that requires client certificates admits a verified client to
/// a private endpoint as its certificate's name, and refuses a client
/// without one, with an untrusted one or with an expired one.
async fn required_client_certificates() {
    let ca = authority("mtls ca");
    let stray = authority("mtls ca");
    let tls = tls_server(&ca, ClientAuth::Required(vec![ca.certificate().clone()]));
    let server = server(tls, SecurityConfig::default().admit_authenticated_peers()).await;

    let member = client_leaf(&ca, "member");
    let reply = call(presenting(&ca, &member), &server.private).await;
    assert_eq!(reply.expect("verified client").text, "hi");
    let seen = server.seen.lock().expect("lock").clone();
    assert_eq!(
        seen,
        vec![(
            Some("test://member".to_string()),
            Some(format!("test://member@{:?}", Some(PEER_IDENTITY_ISSUER)))
        )]
    );

    // No certificate, an untrusted one (another authority with the same
    // name), an expired one: refused, never executed.
    assert!(call(trusting(&ca), &server.private).await.is_err());
    assert!(
        call(
            presenting(&ca, &client_leaf(&stray, "member")),
            &server.private
        )
        .await
        .is_err()
    );
    let expired = leaf(
        &ca,
        vec![SubjectName::Uri("test://late".into())],
        Usage::Client,
        Validity::starting(at(NOW - 10 * DAY), DAY),
    );
    assert!(
        call(presenting(&ca, &expired), &server.private)
            .await
            .is_err()
    );
    // A server-only leaf is not a client certificate.
    let wrong_use = leaf(
        &ca,
        vec![SubjectName::Uri("test://server".into())],
        Usage::Server,
        this_year(),
    );
    assert!(
        call(presenting(&ca, &wrong_use), &server.private)
            .await
            .is_err()
    );
    assert_eq!(
        server.seen.lock().expect("lock").len(),
        1,
        "only the member ran"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn required_client_certificates_current_thread() {
    required_client_certificates().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_client_certificates_multi_thread() {
    required_client_certificates().await;
}

/// Optional client certificates: a client without one is anonymous (public
/// endpoints only); a verified one is admitted to private endpoints only on
/// a runtime that trusts peer identities.
async fn optional_client_certificates() {
    let ca = authority("optional ca");
    let roots = vec![ca.certificate().clone()];
    let trusting_server = server(
        tls_server(&ca, ClientAuth::Optional(roots.clone())),
        SecurityConfig::default().admit_authenticated_peers(),
    )
    .await;
    assert!(call(trusting(&ca), &trusting_server.public).await.is_ok());
    assert!(call(trusting(&ca), &trusting_server.private).await.is_err());
    let member = client_leaf(&ca, "member");
    assert!(
        call(presenting(&ca, &member), &trusting_server.private)
            .await
            .is_ok()
    );

    // The identity is there, but this runtime grants nothing from it.
    let strict_server = server(
        tls_server(&ca, ClientAuth::Optional(roots)),
        SecurityConfig::default(),
    )
    .await;
    assert!(
        call(presenting(&ca, &member), &strict_server.private)
            .await
            .is_err()
    );
    assert!(
        call(presenting(&ca, &member), &strict_server.public)
            .await
            .is_ok()
    );
    let seen = strict_server.seen.lock().expect("lock").clone();
    assert_eq!(seen, vec![(Some("test://member".to_string()), None)]);
}

#[tokio::test(flavor = "current_thread")]
async fn optional_client_certificates_current_thread() {
    optional_client_certificates().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn optional_client_certificates_multi_thread() {
    optional_client_certificates().await;
}

/// The server's client roots and a client's certificate rotate for new
/// handshakes.
async fn client_trust_and_certificates_rotate() {
    let old = authority("old ca");
    let new = authority("new ca");
    let tls = tls_server(&old, ClientAuth::Required(vec![old.certificate().clone()]));
    let rotate = tls.clone();
    let server = server(tls, SecurityConfig::default().admit_authenticated_peers()).await;

    let client = presenting(&old, &client_leaf(&old, "a"));
    assert!(call(client.clone(), &server.private).await.is_ok());

    // The server now trusts only the new authority: the old leaf fails.
    rotate
        .replace_client_roots([new.certificate().clone()])
        .expect("roots");
    assert!(call(client.clone(), &server.private).await.is_err());

    // The client rotates its certificate to one the new authority signed.
    let renewed = client_leaf(&new, "a");
    client
        .certificate()
        .expect("presents one")
        .replace(renewed.chain.clone(), renewed.key())
        .expect("rotate");
    assert!(call(client, &server.private).await.is_ok());
    assert!(matches!(rotate.client_auth(), ClientAuth::Required(roots) if roots.len() == 1));
}

#[tokio::test(flavor = "current_thread")]
async fn client_trust_and_certificates_rotate_current_thread() {
    client_trust_and_certificates_rotate().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_trust_and_certificates_rotate_multi_thread() {
    client_trust_and_certificates_rotate().await;
}
