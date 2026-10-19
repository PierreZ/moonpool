//! Mutual TLS over compio: futures-rustls on `CompioTcpStream` (the write-through
//! stream), split by moonpool-rpc with a read and a write in flight at once.
//! Ported from moonpool-rpc's own `tests/mtls.rs`.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use moonpool_compio::CompioProviders;
use moonpool_core::{Detach, Providers, TaskProvider};
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
    _rpc: RpcHandle<CompioProviders>,
    private: ServiceRef<Echo>,
    public: ServiceRef<Echo>,
    seen: Seen,
}

fn serve(providers: &CompioProviders, mut stream: RequestStream<Echo>, seen: &Seen) {
    let seen = Arc::clone(seen);
    providers
        .task()
        .spawn_task("echo", async move {
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
        .detach();
}

async fn server(tls: TlsServer, security: SecurityConfig) -> Server {
    let providers = CompioProviders::new();
    let config = RpcConfig {
        security,
        ..RpcConfig::default()
    };
    let (driver, rpc) =
        RpcDriver::listen_with(providers.clone(), "127.0.0.1:0", config, Tls::server(tls))
            .await
            .expect("bind");
    providers
        .task()
        .spawn_task("rpc-server", async move {
            let _ = driver.run().await;
        })
        .detach();
    let seen = Seen::default();
    let (private, private_stream) = rpc.register::<Echo>(AccessClass::Private).expect("reg");
    let (public, public_stream) = rpc.register::<Echo>(AccessClass::Public).expect("reg");
    serve(&providers, private_stream, &seen);
    serve(&providers, public_stream, &seen);
    Server {
        _rpc: rpc,
        private,
        public,
        seen,
    }
}

async fn call(client: TlsClient, target: &ServiceRef<Echo>) -> Result<Text, RpcError> {
    let providers = CompioProviders::new();
    let (driver, rpc) =
        RpcDriver::client_only_with(providers.clone(), RpcConfig::default(), Tls::client(client))
            .expect("config");
    providers
        .task()
        .spawn_task("rpc-client", async move {
            let _ = driver.run().await;
        })
        .detach();
    target
        .bind(&rpc)
        .try_get_reply_within(&Text { text: "hi".into() }, CALL)
        .await
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
#[compio::test]
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
    assert_eq!(
        server.seen.lock().expect("lock").len(),
        1,
        "only the member ran"
    );
}

/// Optional client certificates: a client without one reaches public
/// endpoints only; a verified one reaches private endpoints.
#[compio::test]
async fn optional_client_certificates() {
    let ca = authority("optional ca");
    let roots = vec![ca.certificate().clone()];
    let server = server(
        tls_server(&ca, ClientAuth::Optional(roots)),
        SecurityConfig::default().admit_authenticated_peers(),
    )
    .await;
    assert!(call(trusting(&ca), &server.public).await.is_ok());
    assert!(call(trusting(&ca), &server.private).await.is_err());
    let member = client_leaf(&ca, "member");
    assert!(
        call(presenting(&ca, &member), &server.private)
            .await
            .is_ok()
    );
}

/// The server's client roots and a client's certificate rotate for new
/// handshakes.
#[compio::test]
async fn client_trust_and_certificates_rotate() {
    let old = authority("old ca");
    let new = authority("new ca");
    let tls = tls_server(&old, ClientAuth::Required(vec![old.certificate().clone()]));
    let rotate = tls.clone();
    let server = server(tls, SecurityConfig::default().admit_authenticated_peers()).await;

    let client = presenting(&old, &client_leaf(&old, "a"));
    assert!(call(client.clone(), &server.private).await.is_ok());
    rotate
        .replace_client_roots([new.certificate().clone()])
        .expect("roots");
    assert!(call(client.clone(), &server.private).await.is_err());
    let renewed = client_leaf(&new, "a");
    client
        .certificate()
        .expect("presents one")
        .replace(renewed.chain.clone(), renewed.key())
        .expect("rotate");
    assert!(call(client, &server.private).await.is_ok());
}
