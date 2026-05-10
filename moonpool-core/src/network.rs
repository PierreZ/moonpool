//! Network provider abstraction for simulation and real networking.
//!
//! This module provides trait-based networking that allows seamless swapping
//! between real Tokio networking and simulated networking for testing.

use async_trait::async_trait;
use futures::io::{AsyncRead, AsyncWrite};
use std::io;
#[cfg(feature = "tokio-providers")]
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

/// Provider trait for creating network connections and listeners.
///
/// Single-core design - no Send bounds needed.
/// Clone allows sharing providers across multiple peers efficiently.
#[async_trait(?Send)]
pub trait NetworkProvider: Clone {
    /// The TCP stream type for this provider.
    type TcpStream: AsyncRead + AsyncWrite + Unpin + 'static;
    /// The TCP listener type for this provider.
    type TcpListener: TcpListenerTrait<TcpStream = Self::TcpStream> + 'static;

    /// Create a TCP listener bound to the given address.
    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener>;

    /// Connect to a remote address.
    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream>;
}

/// Trait for TCP listeners that can accept connections.
#[async_trait(?Send)]
pub trait TcpListenerTrait {
    /// The TCP stream type that this listener produces.
    type TcpStream: AsyncRead + AsyncWrite + Unpin + 'static;

    /// Accept a single incoming connection.
    async fn accept(&self) -> io::Result<(Self::TcpStream, String)>;

    /// Get the local address this listener is bound to.
    fn local_addr(&self) -> io::Result<String>;
}

/// Real Tokio networking implementation.
///
/// Wraps `tokio::net::TcpStream` with `tokio_util::compat::Compat` so it
/// implements the runtime-agnostic `futures::io::AsyncRead + AsyncWrite` traits
/// required by [`NetworkProvider`].
#[cfg(feature = "tokio-providers")]
#[derive(Debug, Clone)]
pub struct TokioNetworkProvider;

#[cfg(feature = "tokio-providers")]
impl TokioNetworkProvider {
    /// Create a new Tokio network provider.
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "tokio-providers")]
impl Default for TokioNetworkProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "tokio-providers")]
#[async_trait(?Send)]
impl NetworkProvider for TokioNetworkProvider {
    type TcpStream = Compat<tokio::net::TcpStream>;
    type TcpListener = TokioTcpListener;

    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Ok(TokioTcpListener { inner: listener })
    }

    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream> {
        Ok(tokio::net::TcpStream::connect(addr).await?.compat())
    }
}

/// Wrapper for Tokio TcpListener to implement our trait.
#[cfg(feature = "tokio-providers")]
#[derive(Debug)]
pub struct TokioTcpListener {
    inner: tokio::net::TcpListener,
}

#[cfg(feature = "tokio-providers")]
#[async_trait(?Send)]
impl TcpListenerTrait for TokioTcpListener {
    type TcpStream = Compat<tokio::net::TcpStream>;

    async fn accept(&self) -> io::Result<(Self::TcpStream, String)> {
        let (stream, addr) = self.inner.accept().await?;
        Ok((stream.compat(), addr.to_string()))
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(self.inner.local_addr()?.to_string())
    }
}
