//! Network provider abstraction for simulation and real networking.
//!
//! This module provides trait-based networking that allows seamless swapping
//! between real Tokio networking and simulated networking for testing.

use futures::io::{AsyncRead, AsyncWrite};
use std::io;
#[cfg(feature = "tokio-net")]
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

/// Provider trait for creating network connections and listeners.
///
/// Clone allows sharing providers across multiple peers efficiently.
pub trait NetworkProvider: Clone + Send + Sync + 'static {
    /// The TCP stream type for this provider.
    type TcpStream: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    /// The TCP listener type for this provider.
    type TcpListener: TcpListenerTrait<TcpStream = Self::TcpStream> + 'static;

    /// Create a TCP listener bound to the given address.
    fn bind(
        &self,
        addr: &str,
    ) -> impl std::future::Future<Output = io::Result<Self::TcpListener>> + Send;

    /// Connect to a remote address.
    fn connect(
        &self,
        addr: &str,
    ) -> impl std::future::Future<Output = io::Result<Self::TcpStream>> + Send;
}

/// Trait for TCP listeners that can accept connections.
pub trait TcpListenerTrait: Send + Sync + 'static {
    /// The TCP stream type that this listener produces.
    type TcpStream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// Accept a single incoming connection.
    fn accept(
        &self,
    ) -> impl std::future::Future<Output = io::Result<(Self::TcpStream, String)>> + Send;

    /// Get the local address this listener is bound to.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the local address cannot be retrieved
    /// from the underlying listener.
    fn local_addr(&self) -> io::Result<String>;
}

/// Real Tokio networking implementation.
///
/// Wraps `tokio::net::TcpStream` with `tokio_util::compat::Compat` so it
/// implements the runtime-agnostic `futures::io::AsyncRead + AsyncWrite` traits
/// required by [`NetworkProvider`].
///
/// Every connected and accepted stream has Nagle's algorithm disabled
/// (`TCP_NODELAY`), as `FoundationDB`'s `Net2` does for every connection
/// (`Connection::init`, knob `FLOW_TCP_NODELAY`, default on): request/reply
/// protocols (moonpool-rpc, hyper, tonic) write a small frame and wait for
/// its answer, and Nagle would hold that frame back until the previous
/// segment is acknowledged — up to a delayed-ACK timeout of added latency.
/// Protocols that batch their own writes lose nothing. The setting is
/// best-effort: a platform that refuses it (macOS answers `EINVAL` on a
/// socket its peer already reset) leaves the connection as it is, and the
/// next read or write reports the real error. The simulator models no Nagle
/// delay, so simulated and production timing agree on this point.
#[cfg(feature = "tokio-net")]
#[derive(Debug, Clone, Default)]
pub struct TokioNetworkProvider;

#[cfg(feature = "tokio-net")]
impl TokioNetworkProvider {
    /// Create a new Tokio network provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "tokio-net")]
impl NetworkProvider for TokioNetworkProvider {
    type TcpStream = Compat<tokio::net::TcpStream>;
    type TcpListener = TokioTcpListener;

    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Ok(TokioTcpListener { inner: listener })
    }

    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream> {
        let stream = tokio::net::TcpStream::connect(addr).await?;
        disable_nagle(&stream);
        Ok(stream.compat())
    }
}

/// Disable Nagle's algorithm on `stream`, best-effort (see
/// [`TokioNetworkProvider`]).
#[cfg(feature = "tokio-net")]
fn disable_nagle(stream: &tokio::net::TcpStream) {
    if let Err(error) = stream.set_nodelay(true) {
        tracing::debug!(%error, "TCP_NODELAY not set; the connection is used as it is");
    }
}

/// Wrapper for Tokio `TcpListener` to implement our trait.
#[cfg(feature = "tokio-net")]
#[derive(Debug)]
pub struct TokioTcpListener {
    inner: tokio::net::TcpListener,
}

#[cfg(feature = "tokio-net")]
impl TcpListenerTrait for TokioTcpListener {
    type TcpStream = Compat<tokio::net::TcpStream>;

    async fn accept(&self) -> io::Result<(Self::TcpStream, String)> {
        let (stream, addr) = self.inner.accept().await?;
        disable_nagle(&stream);
        Ok((stream.compat(), addr.to_string()))
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(self.inner.local_addr()?.to_string())
    }
}
