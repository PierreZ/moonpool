//! Server-authenticated TLS sessions (feature `tls`).
//!
//! [`Tls`] is a session upgrade ([`Connector`] and [`Acceptor`]) that runs
//! TLS 1.3 over the provider's stream through `futures-rustls` (rustls with
//! the `ring` provider). It implements no cryptography itself.
//!
//! - **The server is authenticated**: the client verifies the server's
//!   certificate chain against its own roots and the name it expects
//!   ([`ServerNames`]: by default the IP address it dialed, so the
//!   certificate needs an IP subject alternative name; or a fixed name, or
//!   a name per address). An untrusted, expired, not-yet-valid or
//!   wrong-name certificate fails the connect, which fails the calls
//!   waiting on it as `ConnectFailed`, never admitted.
//! - **Clients are not authenticated by TLS**: mutual TLS is deliberately
//!   not provided. A client's identity comes from its per-request
//!   credential ([`RequestVerifier`](super::RequestVerifier)); TLS gives
//!   that credential confidentiality and binds it to the authenticated
//!   server.
//! - Certificates are validated at the time of an injected
//!   [`UtcClock`], never the host clock unless it is
//!   [`SystemUtc`](super::SystemUtc). No session tickets are issued (their
//!   rotation would read the host clock); cryptographic randomness comes
//!   from `ring`, never from the runtime's `RandomProvider`, so a
//!   simulation cannot make TLS ciphertext reproducible and does not try
//!   to.
//! - **Rotation**: [`RotatingCertificate::replace`] swaps the server's
//!   certificate and key for every later handshake;
//!   [`TlsClient::replace_roots`] swaps the client's trust anchors for every
//!   later connection. Established sessions keep what they negotiated.
//!   Session resumption is off on both sides, so every new session sees
//!   the current certificate and trust anchors (RPC sessions are
//!   long-lived; the full handshake is paid once per connection).
//! - A session closed from this side sends a TLS `close_notify` before the
//!   stream closes, so the peer can tell an orderly end from truncation.
//!
//! The protected session carries the ordinary frames, checksums included,
//! and [`PeerContext::is_encrypted`] is `true` on both ends, which is what
//! lets the server accept bearer credentials (see
//! [`SecurityConfig::accept_credentials_over_plaintext`](super::SecurityConfig::accept_credentials_over_plaintext)).

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use futures::io::{AsyncRead, AsyncWrite};
use futures_rustls::rustls::client::ClientConfig;
use futures_rustls::rustls::crypto::{CryptoProvider, ring};
use futures_rustls::rustls::server::{ClientHello, ResolvesServerCert, ServerConfig};
use futures_rustls::rustls::sign::CertifiedKey;
use futures_rustls::rustls::{self as rustls_crate, RootCertStore};
use futures_rustls::{TlsAcceptor, TlsConnector};

pub use futures_rustls::rustls;
pub use futures_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};

use super::UtcClock;
use crate::transport::upgrade::{Acceptor, Connector, PeerContext};

/// A TLS configuration that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("TLS configuration: {0}")]
pub struct TlsError(pub String);

impl From<rustls_crate::Error> for TlsError {
    fn from(error: rustls_crate::Error) -> Self {
        Self(error.to_string())
    }
}

/// Bridges a [`UtcClock`] to rustls' certificate validation time.
struct ValidationTime(Arc<dyn UtcClock>);

impl std::fmt::Debug for ValidationTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ValidationTime")
    }
}

impl rustls_crate::time_provider::TimeProvider for ValidationTime {
    fn current_time(&self) -> Option<rustls_crate::pki_types::UnixTime> {
        self.0.now_utc().map(|now| {
            rustls_crate::pki_types::UnixTime::since_unix_epoch(std::time::Duration::from_secs(
                now.unix_seconds(),
            ))
        })
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// The name a client expects the server's certificate to prove, per
/// dialed address.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ServerNames {
    /// The dialed IP address (the certificate needs a matching IP subject
    /// alternative name). The default: dynamic references carry resolved
    /// addresses only.
    #[default]
    IpAddress,
    /// The same DNS name for every server (a shared service certificate).
    Fixed(String),
    /// A name per dialed address; an address without one is refused.
    ByAddress(BTreeMap<SocketAddr, String>),
}

impl ServerNames {
    fn resolve(&self, peer: &str) -> io::Result<ServerName<'static>> {
        let invalid = |detail: String| io::Error::new(io::ErrorKind::InvalidInput, detail);
        let address = peer
            .parse::<SocketAddr>()
            .map_err(|error| invalid(format!("peer {peer} is not ip:port: {error}")))?;
        let name = match self {
            Self::IpAddress => return Ok(ServerName::IpAddress(address.ip().into())),
            Self::Fixed(name) => name.clone(),
            Self::ByAddress(names) => names
                .get(&address)
                .cloned()
                .ok_or_else(|| invalid(format!("no TLS server name for {address}")))?,
        };
        ServerName::try_from(name).map_err(|error| invalid(error.to_string()))
    }
}

/// The client side: trust anchors, the expected server names and the
/// validation clock.
#[derive(Clone)]
pub struct TlsClient {
    config: Arc<RwLock<Arc<ClientConfig>>>,
    clock: Arc<dyn UtcClock>,
    names: ServerNames,
}

impl std::fmt::Debug for TlsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsClient")
            .field("names", &self.names)
            .finish_non_exhaustive()
    }
}

fn client_config(
    roots: impl IntoIterator<Item = CertificateDer<'static>>,
    clock: &Arc<dyn UtcClock>,
) -> Result<Arc<ClientConfig>, TlsError> {
    let mut store = RootCertStore::empty();
    for root in roots {
        store.add(root)?;
    }
    if store.is_empty() {
        return Err(TlsError("no trust anchor".into()));
    }
    let mut config =
        ClientConfig::builder_with_details(provider(), Arc::new(ValidationTime(Arc::clone(clock))))
            .with_protocol_versions(&[&rustls_crate::version::TLS13])?
            .with_root_certificates(store)
            .with_no_client_auth();
    // Every session verifies the server's current certificate: a resumed
    // session would skip that, and outlive a rotation.
    config.resumption = rustls_crate::client::Resumption::disabled();
    Ok(Arc::new(config))
}

impl TlsClient {
    /// Trust servers whose chains end at one of `roots`, validated at
    /// `clock`'s time, expecting [`ServerNames::IpAddress`].
    ///
    /// # Errors
    ///
    /// [`TlsError`] without a usable root.
    pub fn new(
        roots: impl IntoIterator<Item = CertificateDer<'static>>,
        clock: impl UtcClock,
    ) -> Result<Self, TlsError> {
        let clock: Arc<dyn UtcClock> = Arc::new(clock);
        Ok(Self {
            config: Arc::new(RwLock::new(client_config(roots, &clock)?)),
            clock,
            names: ServerNames::default(),
        })
    }

    /// Expect these server names.
    #[must_use]
    pub fn with_server_names(mut self, names: ServerNames) -> Self {
        self.names = names;
        self
    }

    /// Trust `roots` instead, for every later connection (clones share the
    /// change; established sessions are unaffected).
    ///
    /// # Errors
    ///
    /// [`TlsError`] without a usable root; the current roots stay.
    ///
    /// # Panics
    ///
    /// Only if a thread panicked while replacing the roots.
    pub fn replace_roots(
        &self,
        roots: impl IntoIterator<Item = CertificateDer<'static>>,
    ) -> Result<(), TlsError> {
        let config = client_config(roots, &self.clock)?;
        *self
            .config
            .write()
            .expect("RwLock poisoned: prior task panicked") = config;
        Ok(())
    }

    fn current(&self) -> Arc<ClientConfig> {
        Arc::clone(
            &self
                .config
                .read()
                .expect("RwLock poisoned: prior task panicked"),
        )
    }
}

/// The server's certificate chain and key, replaceable while it serves.
#[derive(Debug)]
pub struct RotatingCertificate {
    current: RwLock<Arc<CertifiedKey>>,
}

impl RotatingCertificate {
    fn load(
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Result<Arc<CertifiedKey>, TlsError> {
        if chain.is_empty() {
            return Err(TlsError("empty certificate chain".into()));
        }
        Ok(Arc::new(CertifiedKey::from_der(chain, key, &provider())?))
    }

    /// Serve `chain` (end-entity first) with `key` from the next handshake
    /// on.
    ///
    /// # Errors
    ///
    /// [`TlsError`] when the chain is empty, the key does not parse or does
    /// not match the certificate; the current certificate stays.
    ///
    /// # Panics
    ///
    /// Only if a thread panicked while replacing the certificate.
    pub fn replace(
        &self,
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Result<(), TlsError> {
        let loaded = Self::load(chain, key)?;
        *self
            .current
            .write()
            .expect("RwLock poisoned: prior task panicked") = loaded;
        tracing::info!(target: "moonpool_rpc::audit", "rpc_tls_certificate_replaced");
        Ok(())
    }
}

impl ResolvesServerCert for RotatingCertificate {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(
            &self
                .current
                .read()
                .expect("RwLock poisoned: prior task panicked"),
        ))
    }
}

/// The server side: a rotating certificate and the validation clock.
#[derive(Clone)]
pub struct TlsServer {
    config: Arc<ServerConfig>,
    certificate: Arc<RotatingCertificate>,
}

impl std::fmt::Debug for TlsServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsServer").finish_non_exhaustive()
    }
}

impl TlsServer {
    /// Serve `chain` (end-entity first) with `key`. No client certificate
    /// is requested and no session is resumable (no ticket, no cache).
    ///
    /// # Errors
    ///
    /// [`TlsError`] when the chain or key cannot be used.
    pub fn new(
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
        clock: impl UtcClock,
    ) -> Result<Self, TlsError> {
        let certificate = Arc::new(RotatingCertificate {
            current: RwLock::new(RotatingCertificate::load(chain, key)?),
        });
        let mut config = ServerConfig::builder_with_details(
            provider(),
            Arc::new(ValidationTime(Arc::new(clock))),
        )
        .with_protocol_versions(&[&rustls_crate::version::TLS13])?
        .with_no_client_auth()
        .with_cert_resolver(Arc::clone(&certificate) as Arc<dyn ResolvesServerCert>);
        // No resumption: every session presents the current certificate.
        config.session_storage = Arc::new(rustls_crate::server::NoServerSessionStorage {});
        config.send_tls13_tickets = 0;
        Ok(Self {
            config: Arc::new(config),
            certificate,
        })
    }

    /// The certificate it serves, to rotate.
    #[must_use]
    pub fn certificate(&self) -> &Arc<RotatingCertificate> {
        &self.certificate
    }
}

/// The TLS session upgrade: pass it to
/// [`RpcDriver::listen_with`](crate::RpcDriver::listen_with) or
/// [`RpcDriver::client_only_with`](crate::RpcDriver::client_only_with).
///
/// A runtime that only dials needs the client side, one that only accepts
/// the server side; one that does both (the usual case: listening runtimes
/// also call) needs both. A missing side refuses that direction.
#[derive(Debug, Clone)]
pub struct Tls {
    client: Option<TlsClient>,
    server: Option<TlsServer>,
}

impl Tls {
    /// Dial with `client`, accept with `server`.
    #[must_use]
    pub fn new(client: TlsClient, server: TlsServer) -> Self {
        Self {
            client: Some(client),
            server: Some(server),
        }
    }

    /// Dial only.
    #[must_use]
    pub fn client(client: TlsClient) -> Self {
        Self {
            client: Some(client),
            server: None,
        }
    }

    /// Accept only.
    #[must_use]
    pub fn server(server: TlsServer) -> Self {
        Self {
            client: None,
            server: Some(server),
        }
    }
}

fn describe(
    version: Option<rustls_crate::ProtocolVersion>,
    suite: Option<rustls_crate::SupportedCipherSuite>,
) -> String {
    format!(
        "{}/{}",
        version.map_or_else(|| "TLS".to_string(), |version| format!("{version:?}")),
        suite.map_or_else(
            || "unknown".to_string(),
            |suite| format!("{:?}", suite.suite())
        )
    )
}

impl<S> Connector<S> for Tls
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = futures_rustls::client::TlsStream<S>;

    fn authenticates_server(&self) -> bool {
        self.client.is_some()
    }

    async fn connect(&self, stream: S, peer: &str) -> io::Result<(Self::Stream, PeerContext)> {
        let client = self.client.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "this runtime has no TLS client configuration",
            )
        })?;
        let name = client.names.resolve(peer)?;
        let identity = name.to_str().into_owned();
        let session = TlsConnector::from(client.current())
            .connect(name, stream)
            .await?;
        let (_, connection) = session.get_ref();
        let protection = describe(
            connection.protocol_version(),
            connection.negotiated_cipher_suite(),
        );
        Ok((
            session,
            PeerContext::new(peer)
                .with_identity(identity)
                .with_protection(protection),
        ))
    }
}

impl<S> Acceptor<S> for Tls
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = futures_rustls::server::TlsStream<S>;

    async fn accept(&self, stream: S, peer: &str) -> io::Result<(Self::Stream, PeerContext)> {
        let server = self.server.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "this runtime has no TLS server configuration",
            )
        })?;
        let session = TlsAcceptor::from(Arc::clone(&server.config))
            .accept(stream)
            .await?;
        let (_, connection) = session.get_ref();
        let protection = describe(
            connection.protocol_version(),
            connection.negotiated_cipher_suite(),
        );
        // No client certificate: the peer has an address, not an identity.
        Ok((session, PeerContext::new(peer).with_protection(protection)))
    }
}
