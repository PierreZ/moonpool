//! Networking: [`NetworkProvider`] and [`TcpListenerTrait`], with
//! [`TokioNetworkProvider`] for production, and their local (non-`Send`)
//! forms [`LocalNetworkProvider`] and [`LocalTcpListener`] for
//! thread-per-core runtimes.
//!
//! Streams implement the runtime-neutral `futures::io::AsyncRead` and
//! `AsyncWrite` traits, so protocol code (and hyper, tonic and moonpool-rpc
//! on top of it) never names a runtime. In simulation,
//! `moonpool_sim::SimNetworkProvider` delivers bytes through a simulated
//! network with latency, partitions, clogs, bit flips, random closes and
//! connect failures, all drawn from the seed.
//!
//! ```rust
//! use moonpool_core::{NetworkProvider, TcpListenerTrait};
//!
//! /// Accept one connection and return the peer's address.
//! async fn accept_one<N: NetworkProvider>(network: &N, addr: &str) -> std::io::Result<String> {
//!     let listener = network.bind(addr).await?;
//!     let (_stream, peer) = listener.accept().await?;
//!     Ok(peer)
//! }
//! ```

use futures::io::{AsyncRead, AsyncWrite};
use std::io;
#[cfg(feature = "tokio-net")]
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

/// Provider trait for creating TCP connections and listeners.
///
/// Addresses are `host:port` strings. Production uses
/// [`TokioNetworkProvider`]. Simulation uses `moonpool_sim::SimNetworkProvider`,
/// where each simulated process has its own IP. The provider is cheap to
/// clone and share between components.
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

/// Local (non-`Send`) form of [`NetworkProvider`], for thread-per-core
/// runtimes whose sockets and futures cannot leave their thread.
///
/// The same contract as [`NetworkProvider`] without any `Send` or `Sync`
/// bound: not on the provider, not on its stream type, not on the futures
/// [`bind`](Self::bind) and [`connect`](Self::connect) return. Every
/// [`NetworkProvider`] implements it through a blanket impl, so the Tokio and
/// simulated providers get it for free; a thread-per-core backend implements
/// only this trait.
///
/// Code generic over this trait runs on every backend, and when the backend
/// is `Send` (Tokio, the simulator) the futures it builds are `Send` too:
/// the auto traits of the concrete future leak through the generic code.
///
/// A concrete [`NetworkProvider`] has both traits' methods under the same
/// names. With both traits in scope, `network.bind(..)` is ambiguous: import
/// one trait per module, or call `LocalNetworkProvider::bind(&network, ..)`.
pub trait LocalNetworkProvider: Clone + 'static {
    /// The TCP stream type for this provider.
    type TcpStream: AsyncRead + AsyncWrite + Unpin + 'static;
    /// The TCP listener type for this provider.
    type TcpListener: LocalTcpListener<TcpStream = Self::TcpStream> + 'static;

    /// Create a TCP listener bound to the given address. See
    /// [`NetworkProvider::bind`].
    fn bind(&self, addr: &str) -> impl std::future::Future<Output = io::Result<Self::TcpListener>>;

    /// Connect to a remote address. See [`NetworkProvider::connect`].
    fn connect(&self, addr: &str)
    -> impl std::future::Future<Output = io::Result<Self::TcpStream>>;
}

/// Local (non-`Send`) form of [`TcpListenerTrait`].
///
/// The same contract without any `Send` or `Sync` bound. Every
/// [`TcpListenerTrait`] implements it through a blanket impl. As with
/// [`LocalNetworkProvider`], a concrete listener has both traits' methods
/// under the same names: import one trait per module, or call
/// `LocalTcpListener::accept(&listener)`.
pub trait LocalTcpListener: 'static {
    /// The TCP stream type that this listener produces.
    type TcpStream: AsyncRead + AsyncWrite + Unpin + 'static;

    /// Accept a single incoming connection. See [`TcpListenerTrait::accept`].
    fn accept(&self) -> impl std::future::Future<Output = io::Result<(Self::TcpStream, String)>>;

    /// Get the local address this listener is bound to. See
    /// [`TcpListenerTrait::local_addr`].
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the local address cannot be retrieved
    /// from the underlying listener.
    fn local_addr(&self) -> io::Result<String>;
}

impl<N: NetworkProvider> LocalNetworkProvider for N {
    type TcpStream = N::TcpStream;
    type TcpListener = N::TcpListener;

    fn bind(&self, addr: &str) -> impl std::future::Future<Output = io::Result<Self::TcpListener>> {
        NetworkProvider::bind(self, addr)
    }

    fn connect(
        &self,
        addr: &str,
    ) -> impl std::future::Future<Output = io::Result<Self::TcpStream>> {
        NetworkProvider::connect(self, addr)
    }
}

impl<L: TcpListenerTrait> LocalTcpListener for L {
    type TcpStream = L::TcpStream;

    fn accept(&self) -> impl std::future::Future<Output = io::Result<(Self::TcpStream, String)>> {
        TcpListenerTrait::accept(self)
    }

    fn local_addr(&self) -> io::Result<String> {
        TcpListenerTrait::local_addr(self)
    }
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
    /// Create a Tokio network provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "tokio-net")]
impl NetworkProvider for TokioNetworkProvider {
    type TcpStream = Compat<tokio::net::TcpStream>;
    type TcpListener = TokioTcpListener;

    #[tracing::instrument(skip(self))]
    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Ok(TokioTcpListener { inner: listener })
    }

    #[tracing::instrument(skip(self))]
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

/// The listener [`TokioNetworkProvider::bind`](NetworkProvider::bind) returns: a
/// Tokio `TcpListener` whose accepted streams have `TCP_NODELAY` set.
#[cfg(feature = "tokio-net")]
#[derive(Debug)]
pub struct TokioTcpListener {
    inner: tokio::net::TcpListener,
}

#[cfg(feature = "tokio-net")]
impl TcpListenerTrait for TokioTcpListener {
    type TcpStream = Compat<tokio::net::TcpStream>;

    #[tracing::instrument(skip(self))]
    async fn accept(&self) -> io::Result<(Self::TcpStream, String)> {
        let (stream, addr) = self.inner.accept().await?;
        disable_nagle(&stream);
        Ok((stream.compat(), addr.to_string()))
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(self.inner.local_addr()?.to_string())
    }
}
