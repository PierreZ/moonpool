//! Endpoint identity: transport incarnations, checked tokens and addresses.
//!
//! A dynamic [`Endpoint`] names one receiving endpoint inside one transport
//! incarnation, plus the address that reaches it:
//!
//! - the **address** routes the request;
//! - the [`Incarnation`] is drawn from the provider's random source when a
//!   transport starts, so a restarted process at the same `ip:port` is a
//!   different incarnation and rejects references to its predecessor;
//! - the [`EndpointToken`] is a registry slot plus a checked generation, so
//!   a slot reused after its receiver was dropped never answers to the old
//!   token (no ABA).
//!
//! None of these is a credential. Incarnations are 64 random bits: two
//! independently started transports collide with probability about
//! `n² / 2^65` for `n` live incarnations, which the protocol accepts.
//! Applications needing durable identity or fencing carry their own.

pub(crate) mod registry;

/// One transport incarnation: fresh random material per transport start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Incarnation(u64);

impl Incarnation {
    /// Wrap raw incarnation material (tests, fixtures, decoding).
    ///
    /// A running transport draws its own with
    /// [`RandomProvider`](moonpool_core::RandomProvider); this constructor
    /// exists for data that already names one.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw 64-bit value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Incarnation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// A registry slot and the generation it held when the endpoint registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointToken {
    index: u32,
    generation: u32,
}

impl EndpointToken {
    /// Build a token from its parts (tests and fixtures).
    #[must_use]
    pub const fn from_parts(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    /// The registry slot.
    #[must_use]
    pub const fn index(self) -> u32 {
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
        write!(f, "{}#{}", self.index, self.generation)
    }
}

/// A fully addressed dynamic endpoint: plain, runtime-free routing data.
///
/// It holds no reference to a runtime and keeps nothing alive. It travels
/// inside a [`ServiceRef`](crate::ServiceRef), whose byte layout is fixed by
/// this crate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Endpoint {
    address: String,
    incarnation: Incarnation,
    token: EndpointToken,
}

impl Endpoint {
    /// Assemble an endpoint from its parts.
    #[must_use]
    pub fn new(address: impl Into<String>, incarnation: Incarnation, token: EndpointToken) -> Self {
        Self {
            address: address.into(),
            incarnation,
            token,
        }
    }

    /// The `ip:port` that reaches the owning transport.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The owning transport's incarnation.
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
