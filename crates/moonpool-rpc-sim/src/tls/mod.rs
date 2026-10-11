//! The `sim-rpc-tls` campaign: TLS 1.3 sessions, mutual or not, under the
//! simulation, with the same `security::tls` code production runs.
//!
//! Every seed draws a client authentication mode for the server (off,
//! optional, required) and makes its certificates with the production
//! authority (`security::tls::pki`) from seeded key bytes, so a seed's
//! certificates are the same on every run. The authority's own fault sites
//! issue flawed leaves (short-lived, not valid yet, a name dropped, the
//! wrong use), and the TLS layer's sites abort handshakes, stop at its
//! hints and skip `close_notify`.
//!
//! Roles:
//!
//! - **server** ([`TlsServerProcess`]): accepts with the seed's mode and
//!   trusts authenticated peer identities for its private endpoint.
//! - **impostor** ([`ImpostorProcess`]): a server whose certificate a stray
//!   authority with the same name signed, for the same names.
//! - **workload** ([`TlsWorkload`]): three client runtimes that trust the
//!   real authority: a member presenting its certificate, a stranger
//!   presenting one from the stray authority (or none), and an anonymous
//!   client.
//!
//! **Oracles.** A private endpoint only ever runs a request whose session
//! proved the member's certificate; nothing that trusts the real authority
//! ever reaches the impostor; a reply always echoes its own request. Once
//! the script stops and restores clean certificates, the member reaches the
//! private endpoint again (when the mode verifies clients) and anyone
//! reaches the public one.
//!
//! **Faults.** Swarm network chaos **with bit flips**: under TLS a flipped
//! bit is an authentication failure that ends the session, never a
//! corrupted delivery. The script ([`TlsFaults`]) moves the UTC (steps and
//! jumps past certificate lifetimes), rotates the server's and the member's
//! certificates, moves the server's client roots to an authority nobody
//! holds a leaf from for a while, and crashes the server.

mod faults;
mod server;
mod workload;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_rpc::security::tls::ClientAuth;
use moonpool_rpc::security::tls::pki::{
    Authority, Issued, KeySeed, LeafSpec, SubjectName, Usage, Validity,
};
use moonpool_rpc::security::{FixedUtc, UtcTime};
use moonpool_rpc::{MethodId, RpcMethod, SchemaVersion};
use moonpool_sim::{
    Chaos, ChaosMode, RandomProvider, SimulationBuilder, SimulationError, SimulationResult,
    StateHandle,
};

pub use faults::TlsFaults;
pub use server::{ImpostorProcess, TlsServerProcess};
pub use workload::TlsWorkload;

use crate::lock;

/// The scripted UTC at the start of every run (2030-03-17).
const START_UTC: u64 = 1_900_000_000;
/// A normal leaf's lifetime.
const LEAF_LIFETIME: u64 = 6 * 3600;
/// The authorities' common name: the stray one copies it.
const AUTHORITY_NAME: &str = "moonpool rpc tls ca";
/// The member's identity, as its certificate's URI name.
pub const MEMBER: &str = "rpc://member";
/// The server's identity.
pub const SERVER: &str = "rpc://server";

const PKI_KEY: &str = "rpc.tls.pki";
/// The server's latest references, as [`Refs`].
const SERVER_REFS_KEY: &str = "rpc.tls.server.refs";
/// The impostor's references, as [`Refs`].
const IMPOSTOR_REFS_KEY: &str = "rpc.tls.impostor.refs";
/// Set once the fault script stopped and restored clean certificates.
const SCRIPT_DONE_KEY: &str = "rpc.tls.script.done";

/// Who sent a probe (the harness's own truthful tag, never trusted by the
/// runtime under test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sender {
    /// Presents the member's certificate.
    Member,
    /// Presents a stray authority's certificate, or none.
    Stranger,
    /// Presents nothing.
    Anonymous,
}

impl Sender {
    const ALL: [Self; 3] = [Self::Member, Self::Stranger, Self::Anonymous];

    fn tag(self) -> u32 {
        match self {
            Self::Member => 1,
            Self::Stranger => 2,
            Self::Anonymous => 3,
        }
    }

    fn of(tag: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|sender| sender.tag() == tag)
    }
}

/// One probe.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Probe {
    /// Unique per request.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// [`Sender`] tag.
    #[prost(uint32, tag = "2")]
    pub sender: u32,
}

/// The private echo: only an authenticated member may call it.
pub struct PrivateEcho;
impl RpcMethod for PrivateEcho {
    type Request = Probe;
    type Reply = Probe;
    const METHOD: MethodId = MethodId::new(0x7151_0001);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "tls.private_echo";
}

/// The public echo: anyone who completes the handshake.
pub struct PublicEcho;
impl RpcMethod for PublicEcho {
    type Request = Probe;
    type Reply = Probe;
    const METHOD: MethodId = MethodId::new(0x7151_0002);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "tls.public_echo";
}

/// Serialised references a server boot published.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Refs {
    private: Vec<u8>,
    public: Vec<u8>,
}

/// The seed's client authentication mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No client certificate requested.
    Off,
    /// Verified when presented.
    Optional,
    /// Mutual TLS.
    Required,
}

impl Mode {
    fn client_auth(
        self,
        roots: Vec<moonpool_rpc::security::tls::CertificateDer<'static>>,
    ) -> ClientAuth {
        match self {
            Self::Off => ClientAuth::Off,
            Self::Optional => ClientAuth::Optional(roots),
            Self::Required => ClientAuth::Required(roots),
        }
    }
}

struct Inner {
    clock: FixedUtc,
    utc: u64,
    mode: Mode,
    server_ip: String,
    authority: Authority,
    /// A third authority nobody holds a leaf from: the server's client
    /// roots move to it for a while (a rotation that went wrong).
    unused: Authority,
    server: Issued,
    member: Issued,
    stranger: Option<Issued>,
    impostor: Issued,
    /// Whether the server's client roots are the unused authority's.
    roots_swapped: bool,
    server_tls: Option<moonpool_rpc::security::tls::TlsServer>,
    member_tls: Option<moonpool_rpc::security::tls::TlsClient>,
}

/// The seed's certificates, clock and live TLS handles, shared by every
/// role and the script.
#[derive(Clone)]
pub struct Pki(Arc<Mutex<Inner>>);

fn seed(random: &impl RandomProvider) -> KeySeed {
    let mut bytes = [0u8; 32];
    for chunk in bytes.chunks_mut(8) {
        chunk.copy_from_slice(&random.random::<u64>().to_le_bytes());
    }
    KeySeed::Bytes(bytes)
}

fn invalid(error: impl std::fmt::Display) -> SimulationError {
    SimulationError::InvalidState(format!("tls pki: {error}"))
}

fn issue(
    authority: &Authority,
    names: Vec<SubjectName>,
    usage: Usage,
    utc: u64,
    random: &impl RandomProvider,
) -> SimulationResult<Issued> {
    authority
        .issue(&LeafSpec {
            names,
            validity: Validity::starting(UtcTime::from_unix_seconds(utc - 60), LEAF_LIFETIME),
            usage,
            seed: seed(random),
        })
        .map_err(invalid)
}

fn ip_name(ip: &str) -> SimulationResult<SubjectName> {
    Ok(SubjectName::Ip(ip.parse().map_err(invalid)?))
}

impl Pki {
    /// The run's PKI, made on first use from the seed.
    ///
    /// # Errors
    ///
    /// When a certificate cannot be made.
    pub fn of(
        state: &StateHandle,
        random: &impl RandomProvider,
        server_ip: &str,
        impostor_ip: &str,
    ) -> SimulationResult<Self> {
        if let Some(pki) = state.get::<Self>(PKI_KEY) {
            return Ok(pki);
        }
        let validity =
            Validity::starting(UtcTime::from_unix_seconds(START_UTC - 3600), 400 * 86_400);
        let authority = Authority::new(AUTHORITY_NAME, validity, seed(random)).map_err(invalid)?;
        let stray = Authority::new(AUTHORITY_NAME, validity, seed(random)).map_err(invalid)?;
        let unused = Authority::new(AUTHORITY_NAME, validity, seed(random)).map_err(invalid)?;
        let mode = match random.random_range(0..3u8) {
            0 => Mode::Off,
            1 => Mode::Optional,
            _ => Mode::Required,
        };
        let server_names = |ip: &str| -> SimulationResult<Vec<SubjectName>> {
            Ok(vec![SubjectName::Uri(SERVER.into()), ip_name(ip)?])
        };
        let member_names = vec![SubjectName::Uri(MEMBER.into())];
        let server = issue(
            &authority,
            server_names(server_ip)?,
            Usage::Server,
            START_UTC,
            random,
        )?;
        let member = issue(
            &authority,
            member_names.clone(),
            Usage::Client,
            START_UTC,
            random,
        )?;
        let stranger = if random.random_bool(0.7) {
            Some(issue(
                &stray,
                member_names,
                Usage::Client,
                START_UTC,
                random,
            )?)
        } else {
            None
        };
        let impostor = issue(
            &stray,
            server_names(impostor_ip)?,
            Usage::Server,
            START_UTC,
            random,
        )?;
        tracing::info!(?mode, stranger = stranger.is_some(), "rpc_tls_pki_ready");
        let pki = Self(Arc::new(Mutex::new(Inner {
            clock: FixedUtc::new(UtcTime::from_unix_seconds(START_UTC)),
            utc: START_UTC,
            mode,
            server_ip: server_ip.to_string(),
            authority,
            unused,
            server,
            member,
            stranger,
            impostor,
            roots_swapped: false,
            server_tls: None,
            member_tls: None,
        })));
        state.publish(PKI_KEY, pki.clone());
        Ok(pki)
    }

    /// The run's PKI as the topology names it.
    ///
    /// # Errors
    ///
    /// When a certificate cannot be made.
    pub fn of_topology(
        state: &StateHandle,
        random: &impl RandomProvider,
        server: Option<String>,
        impostor: Option<String>,
    ) -> SimulationResult<Self> {
        let server = server.ok_or_else(|| invalid("no server"))?;
        let impostor = impostor.ok_or_else(|| invalid("no impostor"))?;
        Self::of(state, random, &server, &impostor)
    }

    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        f(&mut lock(&self.0))
    }

    /// The seed's mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.with(|inner| inner.mode)
    }

    /// The validation clock every role uses.
    #[must_use]
    pub fn clock(&self) -> FixedUtc {
        self.with(|inner| inner.clock.clone())
    }

    /// The scripted UTC, in seconds.
    #[must_use]
    pub fn utc(&self) -> u64 {
        self.with(|inner| inner.utc)
    }

    fn advance(&self, seconds: u64) {
        self.with(|inner| {
            inner.utc += seconds;
            inner.clock.set(UtcTime::from_unix_seconds(inner.utc));
        });
    }

    fn authority_root(&self) -> moonpool_rpc::security::tls::CertificateDer<'static> {
        self.with(|inner| inner.authority.certificate().clone())
    }

    fn client_roots(&self) -> Vec<moonpool_rpc::security::tls::CertificateDer<'static>> {
        self.with(|inner| {
            if inner.roots_swapped {
                vec![inner.unused.certificate().clone()]
            } else {
                vec![inner.authority.certificate().clone()]
            }
        })
    }
}

/// The campaign's builder without an iteration policy.
#[must_use]
pub fn tls_campaign() -> SimulationBuilder {
    SimulationBuilder::new()
        .processes(1, || Box::new(TlsServerProcess))
        .processes(1, || Box::new(ImpostorProcess))
        .workload_factory(|| Box::new(TlsWorkload::default()))
        .fault_factory(|| Box::new(TlsFaults))
        .enable_chaos([Chaos::Network(ChaosMode::Swarm)])
        .chaos_duration(Duration::from_secs(8))
}
