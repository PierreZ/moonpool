//! Who may call what: request verification, endpoint access policy,
//! address allow lists, key rotation, the UTC validation clock and, behind
//! features, the TLS and JWT adapters.
//!
//! # Trust model
//!
//! - A TCP address or an endpoint token is **not** an identity. An
//!   [`IpAllowList`] restricts which peers can open a session; it grants
//!   nothing.
//! - Server-authenticated TLS (feature `tls`, [`tls`]) authenticates the
//!   **server** to its clients. Clients are not authenticated by the
//!   handshake: mutual TLS is deliberately not provided.
//! - A **client** is identified per request, by a credential it attaches
//!   ([`Credential`], from a [`CredentialSource`]) and the server verifies
//!   ([`RequestVerifier`], for example the JWT/JWKS adapter behind feature
//!   `jwt`, [`jwt`]) into a [`Principal`].
//! - An [`AccessPolicy`] decides, per request, whether that principal (or
//!   an anonymous caller) may call the endpoint. The default
//!   ([`DefaultPolicy`]) lets anyone who passes verification call a
//!   [`Public`](crate::AccessClass::Public) endpoint and requires a verified
//!   principal for a [`Private`](crate::AccessClass::Private) one.
//! - The check runs in admission, after the endpoint's identity and
//!   contract were checked and **before** the request reaches a handler or
//!   a queue. Local calls (a caller in the same runtime) go through the same
//!   check with the same credential, so they get no shortcut.
//!
//! **Private endpoints fail closed by default.** [`SecurityConfig::default`]
//! verifies nothing, so every private endpoint refuses every caller. A
//! deployment that relies on its network instead says so with
//! [`SecurityConfig::trusted_network`], which admits everything and makes
//! no confidentiality or authentication claim.
//!
//! # Clocks and entropy
//!
//! Validation time is a [`UtcClock`], never the runtime's scheduling time;
//! the default [`NoUtc`] knows no time, so expiring credentials fail closed
//! until a clock is chosen ([`SystemUtc`] in production, [`FixedUtc`] in
//! tests and simulation). Cryptographic entropy belongs to the TLS and JWT
//! dependencies; the runtime's `RandomProvider` only draws protocol values
//! (incarnations, jitter).
//!
//! # Redaction
//!
//! A credential's `Debug` shows only its length, and nothing in this crate
//! logs credential bytes, principals or tokens. Denials are traced as audit
//! events (target `moonpool_rpc::audit`, `rpc_request_denied`, after
//! `FoundationDB`'s `AttemptedRPCToPrivatePrevented`) naming the endpoint,
//! the access class, the peer address and the reason code. There is one
//! `warn` event per denial and no built-in throttling: aggregate through the
//! `moonpool_rpc_requests_denied_total{reason}` counter of
//! [`RpcMetrics`](crate::observability::RpcMetrics), and rate-limit a noisy peer in the
//! tracing subscriber.

mod allow;
mod clock;
mod keys;

#[cfg(feature = "jwt")]
pub mod jwt;
#[cfg(feature = "tls")]
pub mod tls;

use std::sync::Arc;

pub use self::allow::{InvalidSubnet, IpAllowList, Subnet};
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub use self::clock::SystemUtc;
pub use self::clock::{FixedUtc, NoUtc, UtcClock, UtcTime};
pub use self::keys::{KeySet, RotatingKeys};
use crate::endpoint::{AccessClass, Endpoint, EndpointToken};
use crate::interface::InterfaceId;
use crate::protocol::{MethodId, SchemaVersion};
use crate::transport::upgrade::PeerContext;

/// How many [`CredentialError`] variants exist (the metrics label bound).
pub(crate) const CREDENTIAL_ERRORS: usize = 13;

/// The default bound of one request's credential section, in bytes.
pub const DEFAULT_MAX_CREDENTIAL_BYTES: usize = 8 * 1024;

/// A bearer credential attached to requests (for example a signed JWT).
///
/// Its bytes never appear in `Debug` output, traces or metrics.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential(Arc<[u8]>);

impl Credential {
    /// A bearer credential with these bytes.
    #[must_use]
    pub fn bearer(bytes: impl Into<Vec<u8>>) -> Self {
        Self(Arc::from(bytes.into()))
    }

    /// The credential's bytes. Handle with care: this is a secret.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// Its length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether it is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credential(<redacted, {} bytes>)", self.0.len())
    }
}

/// What a caller attaches to its requests.
///
/// Asked again for **every** attempt, including each retransmission of a
/// reliable call, so a refreshed token is picked up without restarting the
/// call. It is never called with a runtime lock held, so it may use the
/// runtime (a handle's stats, another call's outcome), but it runs inline
/// on the call path and on the connection's teardown: keep it quick.
///
/// The credential is attached when the request is framed, before the
/// session that carries it is known. The runtime therefore checks again at
/// the last moment, when the frame is about to be written: a request
/// carrying a credential is written only on an encrypted session (unless
/// this runtime opted into
/// [`SecurityConfig::send_credentials_over_plaintext`]), never on a
/// protocol version 1 session and never on an accepted session whose
/// dialer was not authenticated; otherwise the call fails with
/// [`ErrorReason::CredentialWithheld`](crate::ErrorReason::CredentialWithheld)
/// and the credential never leaves the process.
///
/// A bearer credential is replayable by whoever receives it: any server
/// that accepts the same issuer and audience accepts a token presented to
/// another. Scope audiences per service, and call only servers you trust
/// with the token (a reference taken from a peer names an address the
/// peer chose; TLS proves the server behind it, not that it is the one
/// you meant).
pub trait CredentialSource: Send + Sync + 'static {
    /// The credential for a request to `target`, or `None` to call
    /// anonymously.
    fn credential(&self, target: &Endpoint) -> Option<Credential>;
}

impl CredentialSource for Credential {
    fn credential(&self, _target: &Endpoint) -> Option<Credential> {
        Some(self.clone())
    }
}

/// Why a credential was not accepted. Carried back to the caller as a
/// one-byte code (protocol version 2), so each variant's code is fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum CredentialError {
    /// No credential, where one is required.
    Missing,
    /// The credential section or the credential does not parse.
    Malformed,
    /// Larger than the server accepts.
    TooLarge,
    /// Signed with an algorithm the server does not accept (including a
    /// symmetric algorithm presented against a public key).
    UnsupportedAlgorithm,
    /// Names a key the server does not know (never known, or rotated out).
    UnknownKey,
    /// The signature does not verify.
    BadSignature,
    /// Issued by an issuer the server does not trust.
    WrongIssuer,
    /// Intended for another audience.
    WrongAudience,
    /// Past its expiry.
    Expired,
    /// Before its not-before time.
    NotYetValid,
    /// The server does not know the current UTC time: nothing that expires
    /// can be accepted.
    ClockUnavailable,
    /// Presented over a session that is not encrypted, to a server that
    /// accepts bearer credentials only over encrypted sessions.
    InsecureTransport,
    /// Refused by the application's verifier for its own reason.
    Rejected,
}

impl CredentialError {
    const ALL: [Self; CREDENTIAL_ERRORS] = [
        Self::Missing,
        Self::Malformed,
        Self::TooLarge,
        Self::UnsupportedAlgorithm,
        Self::UnknownKey,
        Self::BadSignature,
        Self::WrongIssuer,
        Self::WrongAudience,
        Self::Expired,
        Self::NotYetValid,
        Self::ClockUnavailable,
        Self::InsecureTransport,
        Self::Rejected,
    ];

    /// Every error, in code order (a bounded label set for metrics).
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &Self::ALL
    }

    /// The error's wire code (1-based).
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Missing => 1,
            Self::Malformed => 2,
            Self::TooLarge => 3,
            Self::UnsupportedAlgorithm => 4,
            Self::UnknownKey => 5,
            Self::BadSignature => 6,
            Self::WrongIssuer => 7,
            Self::WrongAudience => 8,
            Self::Expired => 9,
            Self::NotYetValid => 10,
            Self::ClockUnavailable => 11,
            Self::InsecureTransport => 12,
            Self::Rejected => 13,
        }
    }

    /// The error with wire code `code`.
    #[must_use]
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|error| error.code() == code)
    }

    /// A short, fixed name (the metrics label and the audit reason).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Malformed => "malformed",
            Self::TooLarge => "too_large",
            Self::UnsupportedAlgorithm => "unsupported_algorithm",
            Self::UnknownKey => "unknown_key",
            Self::BadSignature => "bad_signature",
            Self::WrongIssuer => "wrong_issuer",
            Self::WrongAudience => "wrong_audience",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::ClockUnavailable => "clock_unavailable",
            Self::InsecureTransport => "insecure_transport",
            Self::Rejected => "rejected",
        }
    }
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a request was refused by the security check. Always before any
/// handler saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Denial {
    /// The caller is not (validly) authenticated for this endpoint.
    Unauthenticated(CredentialError),
    /// The caller is authenticated, but the policy does not let it call
    /// this endpoint.
    PermissionDenied,
}

impl std::fmt::Display for Denial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthenticated(reason) => write!(f, "unauthenticated ({reason})"),
            Self::PermissionDenied => f.write_str("permission denied"),
        }
    }
}

/// A verified caller: what a [`RequestVerifier`] established from a
/// credential. The meaning of the subject and scopes is the application's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    subject: String,
    issuer: Option<String>,
    scopes: Vec<String>,
    expires_at: Option<UtcTime>,
}

impl Principal {
    /// A principal named `subject`.
    #[must_use]
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            issuer: None,
            scopes: Vec::new(),
            expires_at: None,
        }
    }

    /// With the issuer that vouched for it.
    #[must_use]
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(issuer.into());
        self
    }

    /// With the scopes its credential grants.
    #[must_use]
    pub fn with_scopes(mut self, scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// With the time its credential expires.
    #[must_use]
    pub fn with_expiry(mut self, expires_at: UtcTime) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Who the caller is.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Who vouched for it.
    #[must_use]
    pub fn issuer(&self) -> Option<&str> {
        self.issuer.as_deref()
    }

    /// The scopes granted.
    #[must_use]
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    /// Whether `scope` is granted.
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|granted| granted == scope)
    }

    /// When the credential expires.
    #[must_use]
    pub fn expires_at(&self) -> Option<UtcTime> {
        self.expires_at
    }
}

/// Where a request came from, as far as the transport knows.
#[derive(Debug, Clone, Copy)]
pub enum RequestOrigin<'a> {
    /// A caller in this runtime.
    Local,
    /// A remote caller, over the session this context describes.
    Remote(&'a PeerContext),
}

/// One request presented to the security check.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct AccessRequest<'a> {
    /// Where it came from.
    pub origin: RequestOrigin<'a>,
    /// The endpoint's access class.
    pub access: AccessClass,
    /// The endpoint it is for.
    pub token: EndpointToken,
    /// The endpoint group's interface and version (zero for a single-method
    /// endpoint).
    pub interface: (InterfaceId, SchemaVersion),
    /// The method called.
    pub method: MethodId,
    /// The UTC time of the check, if the configured clock knows it.
    pub now: Option<UtcTime>,
}

/// Turns a presented credential into a [`Principal`], or refuses it.
///
/// Called in admission, synchronously, only for a request that carries a
/// credential; keep it CPU-bound and bounded. Fail closed: anything not
/// positively verified is an error.
pub trait RequestVerifier: Send + Sync + 'static {
    /// Verify `credential` (the raw bytes, a secret: never log them) for
    /// `request`.
    ///
    /// # Errors
    ///
    /// The [`CredentialError`] the caller is told.
    fn verify(
        &self,
        request: &AccessRequest<'_>,
        credential: &[u8],
    ) -> Result<Principal, CredentialError>;
}

/// Decides whether a (possibly anonymous) caller may call an endpoint.
///
/// # Version 1 callers
///
/// A version 1 session carries no credential, so its caller is always
/// anonymous, and a refusal reaches it as the version 1 status that proves
/// the same thing: endpoint not found. Its failure monitor treats that as
/// permanent for the reference. A policy whose answer depends on the time
/// or on state (a schedule, a revocation list, a quota) therefore refuses a
/// version 1 caller for good, until it resolves the reference again, even
/// when it would admit it a moment later.
pub trait AccessPolicy: Send + Sync + 'static {
    /// Allow or refuse `request` from `principal` (`None`: anonymous).
    ///
    /// # Errors
    ///
    /// The [`Denial`] the caller is told.
    fn authorize(
        &self,
        request: &AccessRequest<'_>,
        principal: Option<&Principal>,
    ) -> Result<(), Denial>;
}

/// Public endpoints: anyone who passed verification (anonymous callers
/// included). Private endpoints: a verified principal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DefaultPolicy;

impl AccessPolicy for DefaultPolicy {
    fn authorize(
        &self,
        request: &AccessRequest<'_>,
        principal: Option<&Principal>,
    ) -> Result<(), Denial> {
        match (request.access, principal) {
            (AccessClass::Public, _) | (AccessClass::Private, Some(_)) => Ok(()),
            (AccessClass::Private, None) => Err(Denial::Unauthenticated(CredentialError::Missing)),
        }
    }
}

/// How a runtime decides who may call its endpoints, and what it attaches
/// to its own calls.
///
/// Part of [`RpcConfig`](crate::RpcConfig). Three starting points:
///
/// | Constructor | Public endpoints | Private endpoints | Claims |
/// |---|---|---|---|
/// | [`default`](Self::default) | anyone | nobody (fail closed) | none needed |
/// | [`enforced`](Self::enforced) | anyone passing verification | a verified principal ([`DefaultPolicy`], or [`with_policy`](Self::with_policy)) | per-request authentication |
/// | [`trusted_network`](Self::trusted_network) | anyone | anyone | **none**: whoever can reach the listener is trusted |
///
/// A configuration that verifies credentials requires protocol version 2
/// from its peers (only version 2 carries them), so an older peer is
/// refused at the handshake instead of silently calling without one.
#[derive(Clone)]
pub struct SecurityConfig {
    trusted: bool,
    verifier: Option<Arc<dyn RequestVerifier>>,
    policy: Arc<dyn AccessPolicy>,
    clock: Arc<dyn UtcClock>,
    allow_list: IpAllowList,
    credentials: Option<Arc<dyn CredentialSource>>,
    max_credential_bytes: usize,
    plaintext_credentials: bool,
    send_plaintext: bool,
}

impl Default for SecurityConfig {
    /// Verify nothing: public endpoints answer anyone, private endpoints
    /// refuse everyone (their callers could not be authenticated).
    fn default() -> Self {
        Self {
            trusted: false,
            verifier: None,
            policy: Arc::new(DefaultPolicy),
            clock: Arc::new(NoUtc),
            allow_list: IpAllowList::allow_all(),
            credentials: None,
            max_credential_bytes: DEFAULT_MAX_CREDENTIAL_BYTES,
            plaintext_credentials: false,
            send_plaintext: false,
        }
    }
}

impl SecurityConfig {
    /// Verify request credentials with `verifier` and authorize with
    /// [`DefaultPolicy`].
    #[must_use]
    pub fn enforced(verifier: impl RequestVerifier) -> Self {
        Self {
            verifier: Some(Arc::new(verifier)),
            ..Self::default()
        }
    }

    /// Admit every request to every endpoint, private ones included,
    /// without looking at credentials: the deployment trusts every process
    /// that can reach the listener (`FoundationDB`'s plaintext default).
    ///
    /// This makes **no** confidentiality, peer-authentication or
    /// client-authentication claim. Use it for simulations and closed,
    /// trusted networks only; combine it with an [`IpAllowList`] to at
    /// least restrict who can connect.
    #[must_use]
    pub fn trusted_network() -> Self {
        Self {
            trusted: true,
            ..Self::default()
        }
    }

    /// Authorize with `policy` instead of [`DefaultPolicy`].
    #[must_use]
    pub fn with_policy(mut self, policy: impl AccessPolicy) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    /// Read validation time from `clock` (the default knows no time).
    #[must_use]
    pub fn with_clock(mut self, clock: impl UtcClock) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Accept sessions only from these peers (checked when a connection is
    /// accepted, before any byte is read).
    #[must_use]
    pub fn with_allow_list(mut self, allow_list: IpAllowList) -> Self {
        self.allow_list = allow_list;
        self
    }

    /// Attach credentials from `source` to this runtime's own calls
    /// (a client can override it per client with
    /// [`ServiceClient::with_credentials`](crate::ServiceClient::with_credentials)).
    #[must_use]
    pub fn with_credentials(mut self, source: impl CredentialSource) -> Self {
        self.credentials = Some(Arc::new(source));
        self
    }

    /// Refuse credential sections larger than `bytes` before parsing them.
    #[must_use]
    pub fn with_max_credential_bytes(mut self, bytes: usize) -> Self {
        self.max_credential_bytes = bytes;
        self
    }

    /// Accept bearer credentials over sessions that are not encrypted.
    ///
    /// By default a credential arriving over a plaintext session is
    /// refused ([`CredentialError::InsecureTransport`]): a bearer token is
    /// only as secret as the session carrying it. Simulations and trusted
    /// networks opt in here; the claim is then per-request verification
    /// only, without confidentiality.
    #[must_use]
    pub fn accept_credentials_over_plaintext(mut self) -> Self {
        self.plaintext_credentials = true;
        self
    }

    /// Let this runtime's own requests carry their credentials over
    /// sessions that are not encrypted.
    ///
    /// By default a request carrying a credential is written only on an
    /// encrypted session (a TLS dial); on any other session it fails with
    /// [`ErrorReason::CredentialWithheld`](crate::ErrorReason::CredentialWithheld)
    /// and the credential never leaves the process. Opt in here (the
    /// client-side mirror of
    /// [`accept_credentials_over_plaintext`](Self::accept_credentials_over_plaintext))
    /// for simulations and trusted networks; a
    /// [`trusted_network`](Self::trusted_network) runtime is opted in. A
    /// credential is never written on a protocol version 1 session nor on
    /// an accepted session whose dialer was not authenticated, opt-in or
    /// not.
    #[must_use]
    pub fn send_credentials_over_plaintext(mut self) -> Self {
        self.send_plaintext = true;
        self
    }

    /// Whether this runtime's requests may carry credentials over
    /// unencrypted sessions.
    #[must_use]
    pub fn sends_credentials_over_plaintext(&self) -> bool {
        self.trusted || self.send_plaintext
    }

    /// Whether this is [`trusted_network`](Self::trusted_network).
    #[must_use]
    pub fn is_trusted_network(&self) -> bool {
        self.trusted
    }

    /// Whether requests' credentials are verified (so peers must speak
    /// protocol version 2).
    #[must_use]
    pub fn verifies_credentials(&self) -> bool {
        !self.trusted && self.verifier.is_some()
    }

    /// The configured allow list.
    #[must_use]
    pub fn allow_list(&self) -> &IpAllowList {
        &self.allow_list
    }

    /// The configured bound of one credential section.
    #[must_use]
    pub fn max_credential_bytes(&self) -> usize {
        self.max_credential_bytes
    }

    /// The runtime-wide credential source, if any.
    pub(crate) fn credentials(&self) -> Option<&Arc<dyn CredentialSource>> {
        self.credentials.as_ref()
    }

    /// The configured validation clock's current time.
    #[must_use]
    pub fn now_utc(&self) -> Option<UtcTime> {
        self.clock.now_utc()
    }

    /// The security decision for one request.
    ///
    /// `section` is the request's credential section, or `None` when the
    /// session cannot carry credentials (protocol version 1). Returns the
    /// verified principal (`None`: anonymous or a trusted network).
    ///
    /// # Errors
    ///
    /// The [`Denial`] to send back.
    pub fn decide(
        &self,
        request: &AccessRequest<'_>,
        section: Option<&[u8]>,
    ) -> Result<Option<Principal>, Denial> {
        if self.trusted {
            return Ok(None);
        }
        let credential = match section {
            Some(section) => crate::protocol::metadata::bearer(section, self.max_credential_bytes)
                .map_err(Denial::Unauthenticated)?,
            None => None,
        };
        let principal = match (credential, self.verifier.as_ref()) {
            (Some(credential), Some(verifier)) => {
                if let RequestOrigin::Remote(peer) = request.origin
                    && !peer.is_encrypted()
                    && !self.plaintext_credentials
                {
                    return Err(Denial::Unauthenticated(CredentialError::InsecureTransport));
                }
                Some(
                    verifier
                        .verify(request, credential)
                        .map_err(Denial::Unauthenticated)?,
                )
            }
            // Nothing here can verify it: it grants nothing, the caller is
            // anonymous.
            (Some(_), None) | (None, _) => None,
        };
        self.policy.authorize(request, principal.as_ref())?;
        Ok(principal)
    }
}

impl std::fmt::Debug for SecurityConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecurityConfig")
            .field("trusted_network", &self.trusted)
            .field("verifies_credentials", &self.verifier.is_some())
            .field("allow_list", &self.allow_list)
            .field("attaches_credentials", &self.credentials.is_some())
            .field("max_credential_bytes", &self.max_credential_bytes)
            .field("plaintext_credentials", &self.plaintext_credentials)
            .field("send_plaintext", &self.send_plaintext)
            .finish_non_exhaustive()
    }
}

impl PartialEq for SecurityConfig {
    /// Equal when built from the same verifier, policy, clock and
    /// credential source instances with the same settings.
    fn eq(&self, other: &Self) -> bool {
        fn same<T: ?Sized>(a: &Arc<T>, b: &Arc<T>) -> bool {
            Arc::ptr_eq(a, b)
        }
        self.trusted == other.trusted
            && match (&self.verifier, &other.verifier) {
                (Some(a), Some(b)) => same(a, b),
                (None, None) => true,
                _ => false,
            }
            && same(&self.policy, &other.policy)
            && same(&self.clock, &other.clock)
            && self.allow_list == other.allow_list
            && match (&self.credentials, &other.credentials) {
                (Some(a), Some(b)) => same(a, b),
                (None, None) => true,
                _ => false,
            }
            && self.max_credential_bytes == other.max_credential_bytes
            && self.plaintext_credentials == other.plaintext_credentials
            && self.send_plaintext == other.send_plaintext
    }
}

impl Eq for SecurityConfig {}

/// Emit the audit event for a refused request. Never includes the
/// credential or the principal.
pub(crate) fn audit_denial(request: &AccessRequest<'_>, denial: Denial) {
    let (peer, encrypted) = match request.origin {
        RequestOrigin::Local => ("local", false),
        RequestOrigin::Remote(peer) => (peer.address(), peer.is_encrypted()),
    };
    let reason = match denial {
        Denial::Unauthenticated(error) => error.name(),
        Denial::PermissionDenied => "permission_denied",
    };
    tracing::warn!(
        target: "moonpool_rpc::audit",
        from = peer,
        encrypted,
        token = %request.token,
        method = request.method.get(),
        access = ?request.access,
        reason,
        "rpc_request_denied"
    );
}

#[cfg(test)]
mod tests;
