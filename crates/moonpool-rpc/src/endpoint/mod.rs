//! Endpoint identity: transport incarnations, checked tokens, addresses and
//! access classes.
//!
//! A dynamic [`Endpoint`] names one receiving endpoint inside one transport
//! incarnation, plus the resolved address that reaches it:
//!
//! - the **address** is a resolved `ip:port` ([`SocketAddr`]); hostnames
//!   belong only to well-known bootstrap endpoints (see
//!   [`BootstrapClient`](crate::BootstrapClient));
//! - the [`Incarnation`] is 128 bits drawn from the provider's random source
//!   when a runtime starts (or supplied by the caller), so a restarted
//!   process at the same `ip:port` is a different incarnation and rejects
//!   references to its predecessor;
//! - the [`EndpointToken`] is a 64-bit registry slot plus a checked 32-bit
//!   generation, so a slot reused after its receiver was dropped never
//!   answers to the old token (no ABA);
//! - a [`WellKnownId`] token instead names a fixed bootstrap service that
//!   answers in every incarnation (its admission skips the incarnation
//!   check), so a reference to it survives the server's restart.
//!
//! None of these is a credential. Two independently started runtimes collide
//! with probability about `n² / 2^129` for `n` live incarnations. Durable
//! identity and fencing are the application's.

pub(crate) mod registry;

use std::net::SocketAddr;

/// One transport incarnation: 128 bits of fresh material per runtime start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Incarnation(u128);

impl Incarnation {
    /// Wrap raw incarnation material: a caller-supplied incarnation
    /// ([`RpcConfig::incarnation`](crate::RpcConfig::incarnation)), a
    /// fixture, or a decoded value.
    #[must_use]
    pub const fn from_raw(raw: u128) -> Self {
        Self(raw)
    }

    /// The raw 128-bit value.
    #[must_use]
    pub const fn get(self) -> u128 {
        self.0
    }
}

impl std::fmt::Display for Incarnation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// The id of a well-known endpoint: a fixed, application-chosen number that
/// names the same service in every incarnation of a runtime.
///
/// Well-known endpoints are for bootstrap (a coordinator, a directory) and
/// may come back at the same token after a restart. Everything else should
/// be a dynamic endpoint, whose reference dies with its incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WellKnownId(u32);

impl WellKnownId {
    /// Wrap an application-chosen id.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The raw id.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for WellKnownId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wk{}", self.0)
    }
}

/// Token indices with this bit set name well-known endpoints; the dynamic
/// registry never allocates them.
const WELL_KNOWN_BIT: u64 = 1 << 63;

/// A registry slot and the generation it held when the endpoint registered,
/// or a [`WellKnownId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointToken {
    index: u64,
    generation: u32,
}

impl EndpointToken {
    /// Build a token from its parts (tests and fixtures).
    #[must_use]
    pub const fn from_parts(index: u64, generation: u32) -> Self {
        Self { index, generation }
    }

    /// The token of a well-known endpoint (generation zero, top index bit
    /// set).
    #[must_use]
    pub const fn well_known(id: WellKnownId) -> Self {
        Self {
            index: WELL_KNOWN_BIT | id.0 as u64,
            generation: 0,
        }
    }

    /// Whether the token names a well-known endpoint.
    #[must_use]
    pub const fn is_well_known(self) -> bool {
        self.index & WELL_KNOWN_BIT != 0
    }

    /// The well-known id, if the token names one.
    #[must_use]
    pub fn well_known_id(self) -> Option<WellKnownId> {
        if !self.is_well_known() {
            return None;
        }
        u32::try_from(self.index & !WELL_KNOWN_BIT)
            .ok()
            .map(WellKnownId)
    }

    /// The 64-bit registry slot.
    #[must_use]
    pub const fn index(self) -> u64 {
        self.index
    }

    /// The slot generation this token is valid for.
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation
    }
}

impl std::fmt::Display for EndpointToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.well_known_id() {
            Some(id) => write!(f, "{id}"),
            None => write!(f, "{}#{}", self.index, self.generation),
        }
    }
}

/// Who may call an endpoint.
///
/// Stored with every registration and carried in every
/// [`ServiceRef`](crate::ServiceRef). Not enforced yet: the security
/// package (#218) enforces it; declaring it now keeps references and
/// registrations stable across that change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum AccessClass {
    /// Callable only by trusted peers.
    #[default]
    Private,
    /// Callable by any peer that passes request verification.
    Public,
}

impl AccessClass {
    pub(crate) const fn to_byte(self) -> u8 {
        match self {
            Self::Private => 0,
            Self::Public => 1,
        }
    }

    pub(crate) const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Private),
            1 => Some(Self::Public),
            _ => None,
        }
    }
}

/// A fully addressed dynamic endpoint: plain, runtime-free routing data.
///
/// It holds no reference to a runtime and keeps nothing alive. It travels
/// inside a [`ServiceRef`](crate::ServiceRef), whose byte layout is fixed by
/// this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Endpoint {
    address: SocketAddr,
    incarnation: Incarnation,
    token: EndpointToken,
}

impl Endpoint {
    /// Assemble an endpoint from its parts.
    #[must_use]
    pub const fn new(address: SocketAddr, incarnation: Incarnation, token: EndpointToken) -> Self {
        Self {
            address,
            incarnation,
            token,
        }
    }

    /// The resolved address of the owning runtime.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// The owning runtime's incarnation.
    #[must_use]
    pub const fn incarnation(&self) -> Incarnation {
        self.incarnation
    }

    /// The checked registry token.
    #[must_use]
    pub const fn token(&self) -> EndpointToken {
        self.token
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.address, self.incarnation, self.token)
    }
}
