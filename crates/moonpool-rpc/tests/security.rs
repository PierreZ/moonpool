//! Endpoint access and request verification on real TCP (production
//! providers, both Tokio flavors where it matters).
//!
//! Every handler here counts what it executed (the receipts): a refused
//! request must never show up there, whichever route it took.

#![cfg(feature = "prost")]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_core::TokioProviders;
use moonpool_core::metrics::MetricsSource;
use moonpool_rpc::observability::RpcMetrics;
use moonpool_rpc::security::{
    AccessPolicy, AccessRequest, Credential, CredentialError, Denial, IpAllowList, Principal,
    RequestVerifier, SecurityConfig,
};
use moonpool_rpc::{
    AccessClass, ErrorReason, Execution, IncomingRequest, MethodId, RequestStream, RpcConfig,
    RpcDriver, RpcError, RpcHandle, RpcMethod, SchemaVersion, ServiceRef,
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
    const METHOD: MethodId = MethodId::new(0x5ec0);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}

struct Items;
impl RpcMethod for Items {
    type Request = Text;
    type Reply = Text;
    const METHOD: MethodId = MethodId::new(0x5ec1);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "items";
    const STREAMING: bool = true;
}

const SECRET: &str = "Zm9vYmFy-very-secret-bearer";
const CALL: Duration = Duration::from_secs(5);

/// `SECRET` is principal `alice` with scope `read`; `admin:<x>` is
/// principal `x` with scope `admin`; anything else is a bad signature.
struct Tokens;
impl RequestVerifier for Tokens {
    fn verify(
        &self,
        _request: &AccessRequest<'_>,
        credential: &[u8],
    ) -> Result<Principal, CredentialError> {
        let text = std::str::from_utf8(credential).map_err(|_| CredentialError::Malformed)?;
        if text == SECRET {
            return Ok(Principal::new("alice").with_scopes(["read"]));
        }
        text.strip_prefix("admin:")
            .map(|subject| Principal::new(subject).with_scopes(["admin"]))
            .ok_or(CredentialError::BadSignature)
    }
}

/// Private endpoints need the `admin` scope.
struct AdminsOnly;
impl AccessPolicy for AdminsOnly {
    fn authorize(
        &self,
        request: &AccessRequest<'_>,
        principal: Option<&Principal>,
    ) -> Result<(), Denial> {
        match (request.access, principal) {
            (AccessClass::Public, _) => Ok(()),
            (AccessClass::Private, None) => Err(Denial::Unauthenticated(CredentialError::Missing)),
            (AccessClass::Private, Some(who)) if who.has_scope("admin") => Ok(()),
            (AccessClass::Private, Some(_)) => Err(Denial::PermissionDenied),
        }
    }
}

type Driver = tokio::task::JoinHandle<std::io::Error>;

async fn server(security: SecurityConfig) -> (RpcHandle<TokioProviders>, Driver) {
    let config = RpcConfig {
        security,
        ..RpcConfig::default()
    };
    let (driver, rpc) = RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", config)
        .await
        .expect("bind");
    (rpc, tokio::spawn(driver.run()))
}

fn client(security: SecurityConfig) -> (RpcHandle<TokioProviders>, Driver) {
    let config = RpcConfig {
        security,
        ..RpcConfig::default()
    };
    let (driver, rpc) = RpcDriver::client_only(TokioProviders::new(), config).expect("config");
    (rpc, tokio::spawn(driver.run()))
}

/// Receipts: what each handler executed, and who it saw.
#[derive(Clone, Default)]
struct Receipts {
    executed: Arc<AtomicU32>,
    subjects: Arc<Mutex<Vec<Option<String>>>>,
}

impl Receipts {
    fn executed(&self) -> u32 {
        self.executed.load(Ordering::SeqCst)
    }

    fn subjects(&self) -> Vec<Option<String>> {
        self.subjects
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .clone()
    }
}

fn serve(mut stream: RequestStream<Echo>, receipts: &Receipts) -> tokio::task::JoinHandle<()> {
    let receipts = receipts.clone();
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            receipts.executed.fetch_add(1, Ordering::SeqCst);
            receipts
                .subjects
                .lock()
                .expect("Mutex poisoned: prior task panicked")
                .push(reply.principal().map(|who| who.subject().to_string()));
            let _ = reply.send(&request);
        }
    })
}

fn text(text: &str) -> Text {
    Text { text: text.into() }
}

async fn call(
    rpc: &RpcHandle<TokioProviders>,
    target: &ServiceRef<Echo>,
    credential: Option<&str>,
) -> Result<Text, RpcError> {
    let mut client = target.bind(rpc);
    if let Some(credential) = credential {
        client = client.with_credentials(Credential::bearer(credential));
    }
    client.try_get_reply_within(&text("hi"), CALL).await
}

fn denied(result: &Result<Text, RpcError>) -> Option<ErrorReason> {
    match result {
        Err(error) => {
            assert_eq!(error.execution(), Execution::NotAdmitted, "{error}");
            Some(error.reason().clone())
        }
        Ok(_) => None,
    }
}

/// With the default configuration, private endpoints refuse every caller,
/// remote or local, and public endpoints answer anyone: fail closed.
async fn private_endpoints_fail_closed_by_default() {
    let (server, server_driver) = server(SecurityConfig::default()).await;
    let receipts = Receipts::default();
    let (private, private_stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let (public, public_stream) = server.register::<Echo>(AccessClass::Public).expect("reg");
    let handlers = [
        serve(private_stream, &receipts),
        serve(public_stream, &receipts),
    ];
    let (remote, remote_driver) = client(SecurityConfig::default());
    let missing = Some(ErrorReason::Unauthenticated(CredentialError::Missing));
    for caller in [&remote, &server] {
        assert_eq!(denied(&call(caller, &private, None).await), missing);
        // A credential the server cannot verify grants nothing.
        assert_eq!(denied(&call(caller, &private, Some(SECRET)).await), missing);
        assert_eq!(denied(&call(caller, &public, None).await), None);
    }
    assert_eq!(receipts.executed(), 2, "only the public calls ran");
    assert_eq!(server.stats().expect("running").requests_unauthenticated, 4);
    remote_driver.abort();
    server_driver.abort();
    for handler in handlers {
        handler.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn private_endpoints_fail_closed_by_default_current_thread() {
    private_endpoints_fail_closed_by_default().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_endpoints_fail_closed_by_default_multi_thread() {
    private_endpoints_fail_closed_by_default().await;
}

/// Verified credentials open private endpoints, the same way for a remote
/// and a local caller; bad credentials are refused before any handler,
/// also on a public endpoint.
async fn credentials_are_verified_before_dispatch_on_every_route() {
    let security = SecurityConfig::enforced(Tokens).accept_credentials_over_plaintext();
    let (server, server_driver) = server(security.clone()).await;
    let receipts = Receipts::default();
    let (private, private_stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let (public, public_stream) = server.register::<Echo>(AccessClass::Public).expect("reg");
    let handlers = [
        serve(private_stream, &receipts),
        serve(public_stream, &receipts),
    ];
    let (remote, remote_driver) = client(SecurityConfig::default());
    for caller in [&remote, &server] {
        assert_eq!(denied(&call(caller, &private, Some(SECRET)).await), None);
        assert_eq!(
            denied(&call(caller, &private, Some("forged")).await),
            Some(ErrorReason::Unauthenticated(CredentialError::BadSignature))
        );
        assert_eq!(
            denied(&call(caller, &private, None).await),
            Some(ErrorReason::Unauthenticated(CredentialError::Missing))
        );
        assert_eq!(
            denied(&call(caller, &public, Some("forged")).await),
            Some(ErrorReason::Unauthenticated(CredentialError::BadSignature)),
            "public means verified, not unchecked"
        );
        assert_eq!(denied(&call(caller, &public, None).await), None);
    }
    assert_eq!(receipts.executed(), 4);
    let subjects = receipts.subjects();
    assert_eq!(
        subjects
            .iter()
            .filter(|who| who.as_deref() == Some("alice"))
            .count(),
        2,
        "the handler sees the verified principal, locally too: {subjects:?}"
    );
    // One-way requests pass the same check: the refused ones never run.
    let receipts_before = receipts.executed();
    private
        .bind(&remote)
        .send(&text("one-way"))
        .expect("queued");
    private
        .bind(&remote)
        .with_credentials(Credential::bearer(SECRET))
        .send(&text("one-way"))
        .expect("queued");
    let settled = tokio::time::timeout(CALL, async {
        while receipts.executed() < receipts_before + 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(settled.is_ok());
    // A round trip after both: the refused one-way had its chance to run.
    assert_eq!(denied(&call(&remote, &public, None).await), None);
    assert_eq!(receipts.executed(), receipts_before + 2);
    let stats = server.stats().expect("running");
    assert_eq!(stats.requests_authenticated, 3);
    remote_driver.abort();
    server_driver.abort();
    for handler in handlers {
        handler.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn credentials_are_verified_before_dispatch_current_thread() {
    credentials_are_verified_before_dispatch_on_every_route().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credentials_are_verified_before_dispatch_multi_thread() {
    credentials_are_verified_before_dispatch_on_every_route().await;
}

/// The policy decides after verification: authenticated but not allowed
/// is `PermissionDenied`, distinct from unauthenticated.
#[tokio::test(flavor = "current_thread")]
async fn the_access_policy_decides_after_verification() {
    let security = SecurityConfig::enforced(Tokens)
        .accept_credentials_over_plaintext()
        .with_policy(AdminsOnly);
    let (server, server_driver) = server(security).await;
    let receipts = Receipts::default();
    let (private, stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let handler = serve(stream, &receipts);
    let (remote, remote_driver) = client(SecurityConfig::default());
    assert_eq!(
        denied(&call(&remote, &private, Some(SECRET)).await),
        Some(ErrorReason::PermissionDenied)
    );
    assert_eq!(
        denied(&call(&remote, &private, Some("admin:root")).await),
        None
    );
    assert_eq!(receipts.subjects(), [Some("root".to_string())]);
    let stats = server.stats().expect("running");
    assert_eq!(stats.requests_permission_denied, 1);
    remote_driver.abort();
    server_driver.abort();
    handler.abort();
}

/// A bearer credential over a plaintext session is refused unless the
/// server opted in: it would travel in the clear.
#[tokio::test(flavor = "current_thread")]
async fn bearer_credentials_over_plaintext_need_an_explicit_opt_in() {
    let (server, server_driver) = server(SecurityConfig::enforced(Tokens)).await;
    let receipts = Receipts::default();
    let (private, stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let handler = serve(stream, &receipts);
    let (remote, remote_driver) = client(SecurityConfig::default());
    assert_eq!(
        denied(&call(&remote, &private, Some(SECRET)).await),
        Some(ErrorReason::Unauthenticated(
            CredentialError::InsecureTransport
        ))
    );
    // A local caller crosses no network.
    assert_eq!(denied(&call(&server, &private, Some(SECRET)).await), None);
    assert_eq!(receipts.executed(), 1);
    remote_driver.abort();
    server_driver.abort();
    handler.abort();
}

/// A stream request passes the same check before any item is produced.
#[tokio::test(flavor = "current_thread")]
async fn stream_requests_are_checked_before_a_producer_exists() {
    let security = SecurityConfig::enforced(Tokens).accept_credentials_over_plaintext();
    let (server, server_driver) = server(security).await;
    let (items, mut stream) = server.register::<Items>(AccessClass::Private).expect("reg");
    let produced = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&produced);
    let handler = tokio::spawn(async move {
        while let Some(IncomingRequest { reply, .. }) = stream.recv().await {
            counter.fetch_add(1, Ordering::SeqCst);
            if let Ok(producer) = reply.into_stream() {
                let _ = producer.send(&text("item")).await;
                let _ = producer.finish();
            }
        }
    });
    let (remote, remote_driver) = client(SecurityConfig::default());
    let mut refused = items
        .bind(&remote)
        .get_reply_stream(&text("go"))
        .expect("opened");
    let outcome = tokio::time::timeout(CALL, refused.recv())
        .await
        .expect("an outcome")
        .expect("a terminal item");
    let error = outcome.expect_err("refused");
    assert_eq!(
        error.reason(),
        &ErrorReason::Unauthenticated(CredentialError::Missing)
    );
    assert_eq!(error.execution(), Execution::NotAdmitted);
    let mut allowed = items
        .bind(&remote)
        .with_credentials(Credential::bearer(SECRET))
        .get_reply_stream(&text("go"))
        .expect("opened");
    let item = tokio::time::timeout(CALL, allowed.recv())
        .await
        .expect("an item")
        .expect("not ended")
        .expect("ok");
    assert_eq!(item.text, "item");
    assert_eq!(produced.load(Ordering::SeqCst), 1);
    remote_driver.abort();
    server_driver.abort();
    handler.abort();
}

/// An allow list restricts who can connect, before any byte is read; it
/// authenticates nothing: an allowed address still needs a valid
/// credential for a private endpoint.
#[tokio::test(flavor = "current_thread")]
async fn an_allow_list_restricts_reachability_but_is_not_authentication() {
    let blocked = SecurityConfig::enforced(Tokens)
        .accept_credentials_over_plaintext()
        .with_allow_list(IpAllowList::parse("10.0.0.0/8").expect("list"));
    let (server, server_driver) = self::server(blocked).await;
    let receipts = Receipts::default();
    let (private, stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let handler = serve(stream, &receipts);
    let (remote, remote_driver) = client(SecurityConfig::default());
    let refused = call(&remote, &private, Some(SECRET))
        .await
        .expect_err("not reachable");
    assert_ne!(refused.execution(), Execution::Executed);
    assert!(
        server
            .stats()
            .expect("running")
            .connections_refused_by_policy
            >= 1
    );
    server_driver.abort();
    handler.abort();
    remote_driver.abort();

    let allowed = SecurityConfig::enforced(Tokens)
        .accept_credentials_over_plaintext()
        .with_allow_list(IpAllowList::parse("127.0.0.0/8").expect("list"));
    let (server, server_driver) = self::server(allowed).await;
    let (private, stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let handler = serve(stream, &receipts);
    let (remote, remote_driver) = client(SecurityConfig::default());
    assert_eq!(
        denied(&call(&remote, &private, Some("forged")).await),
        Some(ErrorReason::Unauthenticated(CredentialError::BadSignature)),
        "an allowed address is not an identity"
    );
    assert_eq!(denied(&call(&remote, &private, Some(SECRET)).await), None);
    assert_eq!(receipts.executed(), 1);
    remote_driver.abort();
    server_driver.abort();
    handler.abort();
}

/// A writer the redaction test captures traces into.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("Mutex poisoned: prior task panicked"))
            .into_owned()
    }
}

/// Denials are audited with the endpoint, peer and reason, never the
/// credential; metrics have a fixed label set whatever the traffic.
#[tokio::test(flavor = "current_thread")]
async fn denials_are_audited_and_credentials_never_leak() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let forged = "forged-Zm9vYmFy-secret-material";
    let security = SecurityConfig::enforced(Tokens).accept_credentials_over_plaintext();
    let (server, server_driver) = server(security).await;
    let metrics = RpcMetrics::new(&server).expect("running");
    let series_before: Vec<String> = metrics
        .collect()
        .iter()
        .map(moonpool_core::MetricSample::sort_key)
        .collect();
    let receipts = Receipts::default();
    let (private, stream) = server.register::<Echo>(AccessClass::Private).expect("reg");
    let handler = serve(stream, &receipts);
    let mut clients = Vec::new();
    for _ in 0..3 {
        let (remote, driver) = client(SecurityConfig::default());
        assert!(denied(&call(&remote, &private, Some(forged)).await).is_some());
        assert_eq!(denied(&call(&remote, &private, Some(SECRET)).await), None);
        clients.push((remote, driver));
    }
    let logs = captured.text();
    assert!(logs.contains("rpc_request_denied"), "audited: {logs}");
    assert!(logs.contains("bad_signature"));
    assert!(
        !logs.contains(forged),
        "a refused credential leaked: {logs}"
    );
    assert!(!logs.contains(SECRET), "an accepted credential leaked");

    let samples = metrics.collect();
    let series_after: Vec<String> = samples
        .iter()
        .map(moonpool_core::MetricSample::sort_key)
        .collect();
    assert_eq!(
        series_before, series_after,
        "three peers and six calls add no series"
    );
    for sample in &samples {
        assert!(sample.name.starts_with("moonpool_rpc_"));
        for (key, value) in &sample.labels {
            assert_eq!(key, "reason");
            assert!(!value.contains(forged) && !value.contains(SECRET));
        }
    }
    let bad_signatures = samples
        .iter()
        .find(|sample| {
            sample.name == "moonpool_rpc_requests_denied_total"
                && sample.labels == [("reason".to_string(), "bad_signature".to_string())]
        })
        .map(|sample| sample.value.scalar());
    assert_eq!(bad_signatures, Some(3.0));
    for (_, driver) in clients {
        driver.abort();
    }
    server_driver.abort();
    handler.abort();
}

#[cfg(feature = "jwt")]
mod jwt {
    //! Signed tokens with the JWT/JWKS adapter, over real TCP.

    use jsonwebtoken::jwk::{Jwk, JwkSet};
    use jsonwebtoken::{EncodingKey, Header, encode};
    use moonpool_rpc::security::jwt::{Algorithm, JwksKeys, JwtConfig, JwtVerifier};
    use moonpool_rpc::security::{CredentialError, FixedUtc, SecurityConfig, UtcTime};
    use moonpool_rpc::{AccessClass, ErrorReason};

    use super::{Receipts, call, client, denied, serve, server};

    const NOW: u64 = 1_900_000_000;

    fn key(kid: &str) -> (EncodingKey, Jwk) {
        let pair = rcgen::KeyPair::generate().expect("key");
        let key = EncodingKey::from_ec_der(&pair.serialize_der());
        let mut jwk = Jwk::from_encoding_key(&key, Algorithm::ES256).expect("jwk");
        jwk.common.key_id = Some(kid.into());
        (key, jwk)
    }

    fn token(key: &EncodingKey, kid: &str, audience: &str, exp: u64) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(kid.into());
        let claims = serde_json::json!({
            "sub": "worker-7", "iss": "issuer", "aud": audience, "exp": exp,
        });
        encode(&header, &claims, key).expect("sign")
    }

    /// Successful signed-token calls, wrong client credentials, key
    /// rotation and expiry against the injected UTC clock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signed_tokens_rotation_and_expiry_over_real_tcp() {
        let (_, _) = key("warm-up");
        let (old, old_jwk) = key("old");
        let (new, new_jwk) = key("new");
        let keys = JwksKeys::from_set(&JwkSet {
            keys: vec![old_jwk],
        })
        .expect("keys");
        let verifier =
            JwtVerifier::new(keys.clone(), JwtConfig::new("issuer", "rpc")).expect("verifier");
        let clock = FixedUtc::new(UtcTime::from_unix_seconds(NOW));
        let security = SecurityConfig::enforced(verifier)
            .accept_credentials_over_plaintext()
            .with_clock(clock.clone());
        let (server, server_driver) = server(security).await;
        let receipts = Receipts::default();
        let (private, stream) = server
            .register::<super::Echo>(AccessClass::Private)
            .expect("reg");
        let handler = serve(stream, &receipts);
        let (remote, remote_driver) = client(SecurityConfig::default());

        let old_token = token(&old, "old", "rpc", NOW + 60);
        assert_eq!(
            denied(&call(&remote, &private, Some(&old_token)).await),
            None
        );
        assert_eq!(
            denied(
                &call(
                    &remote,
                    &private,
                    Some(&token(&old, "old", "other", NOW + 60))
                )
                .await
            ),
            Some(ErrorReason::Unauthenticated(CredentialError::WrongAudience))
        );
        let new_token = token(&new, "new", "rpc", NOW + 60);
        assert_eq!(
            denied(&call(&remote, &private, Some(&new_token)).await),
            Some(ErrorReason::Unauthenticated(CredentialError::UnknownKey))
        );
        let rotated = serde_json::to_string(&JwkSet {
            keys: vec![new_jwk],
        })
        .expect("json");
        keys.replace_json(&rotated).expect("rotate");
        assert_eq!(
            denied(&call(&remote, &private, Some(&old_token)).await),
            Some(ErrorReason::Unauthenticated(CredentialError::UnknownKey)),
            "the rotated-out key is revoked, cached or not"
        );
        assert_eq!(
            denied(&call(&remote, &private, Some(&new_token)).await),
            None
        );
        clock.advance(60);
        assert_eq!(
            denied(&call(&remote, &private, Some(&new_token)).await),
            Some(ErrorReason::Unauthenticated(CredentialError::Expired))
        );
        assert_eq!(receipts.executed(), 2);
        assert_eq!(
            receipts.subjects(),
            [Some("worker-7".to_string()), Some("worker-7".to_string())]
        );
        remote_driver.abort();
        server_driver.abort();
        handler.abort();
    }
}
