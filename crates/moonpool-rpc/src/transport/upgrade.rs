//! The connection upgrade seam: what happens to a raw provider stream
//! before the RPC session runs over it.
//!
//! The peer driver never assumes the provider's TCP stream is the session
//! stream. Every outbound stream goes through a [`Connector`] and every
//! accepted one through an [`Acceptor`], which may wrap it (TLS, feature
//! `tls`: [`security::tls`](crate::security)) and report what they learned
//! about the peer as a [`PeerContext`]. The default [`Plaintext`] passes the
//! stream through untouched and knows only the address. Frame checksums
//! stay on whatever the upgrade is.
//!
//! These traits are RPC-local on purpose: `moonpool-core` gains no new
//! provider trait for them.

use std::future::Future;
use std::io;

use futures::io::{AsyncRead, AsyncWrite};

/// What a session upgrade established about the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PeerContext {
    address: String,
    identity: Option<String>,
    protection: Option<String>,
}

impl PeerContext {
    /// A peer known only by its transport address.
    #[must_use]
    pub fn new(address: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            identity: None,
            protection: None,
        }
    }

    /// Attach an identity an upgrade **authenticated** (for example the
    /// server name a TLS client verified). Never set it from anything the
    /// peer merely claimed: nothing in this crate grants trust from it, but
    /// applications read it as authenticated. Plaintext never sets one, and
    /// the server side of a TLS session has none (no client certificates).
    #[must_use]
    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = Some(identity.into());
        self
    }

    /// Record that the session is encrypted and integrity-protected, and
    /// how (for example `"TLSv1_3/TLS13_AES_256_GCM_SHA384"`). Bearer
    /// credentials are accepted only over such sessions unless the server
    /// opts out
    /// ([`SecurityConfig::accept_credentials_over_plaintext`](crate::security::SecurityConfig::accept_credentials_over_plaintext)).
    #[must_use]
    pub fn with_protection(mut self, protection: impl Into<String>) -> Self {
        self.protection = Some(protection.into());
        self
    }

    /// Whether the session is encrypted.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.protection.is_some()
    }

    /// How the session is protected, if it is.
    #[must_use]
    pub fn protection(&self) -> Option<&str> {
        self.protection.as_deref()
    }

    /// The peer's transport address. Unauthenticated: an address is not an
    /// identity.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The identity the upgrade authenticated, if any.
    #[must_use]
    pub fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }
}

/// Upgrades an outbound provider stream `S` into a session stream.
pub trait Connector<S>: Send + Sync + 'static {
    /// The upgraded stream the session runs over.
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// Upgrade `stream`, freshly connected to `peer`.
    ///
    /// The runtime bounds this (with the connect) by
    /// [`RpcConfig::connect_timeout`](crate::RpcConfig::connect_timeout).
    fn connect(
        &self,
        stream: S,
        peer: &str,
    ) -> impl Future<Output = io::Result<(Self::Stream, PeerContext)>> + Send;
}

/// Upgrades an accepted provider stream `S` into a session stream.
pub trait Acceptor<S>: Send + Sync + 'static {
    /// The upgraded stream the session runs over.
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// Upgrade `stream`, just accepted from `peer`.
    ///
    /// The runtime bounds this by
    /// [`RpcConfig::handshake_timeout`](crate::RpcConfig::handshake_timeout).
    fn accept(
        &self,
        stream: S,
        peer: &str,
    ) -> impl Future<Output = io::Result<(Self::Stream, PeerContext)>> + Send;
}

/// No upgrade: the session runs over the provider stream as is.
///
/// Confidentiality and peer authentication are not provided; integrity
/// against accidental corruption still is, by the frame checksums.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Plaintext;

impl<S> Connector<S> for Plaintext
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = S;

    async fn connect(&self, stream: S, peer: &str) -> io::Result<(S, PeerContext)> {
        Ok((stream, PeerContext::new(peer)))
    }
}

impl<S> Acceptor<S> for Plaintext
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = S;

    async fn accept(&self, stream: S, peer: &str) -> io::Result<(S, PeerContext)> {
        Ok((stream, PeerContext::new(peer)))
    }
}
