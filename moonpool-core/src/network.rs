//! Network provider abstraction for simulation and real networking.
//!
//! This module provides trait-based networking that allows seamless swapping
//! between real Tokio networking and simulated networking for testing.

use async_trait::async_trait;
use std::io;
use tokio::io::{AsyncRead, AsyncWrite};

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
#[derive(Debug, Clone)]
pub struct TokioNetworkProvider;

impl TokioNetworkProvider {
    /// Create a new Tokio network provider.
    pub fn new() -> Self {
        Self
    }
}

impl Default for TokioNetworkProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl NetworkProvider for TokioNetworkProvider {
    type TcpStream = tokio::net::TcpStream;
    type TcpListener = TokioTcpListener;

    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Ok(TokioTcpListener { inner: listener })
    }

    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream> {
        tokio::net::TcpStream::connect(addr).await
    }
}

/// Wrapper for Tokio TcpListener to implement our trait.
#[derive(Debug)]
pub struct TokioTcpListener {
    inner: tokio::net::TcpListener,
}

#[async_trait(?Send)]
impl TcpListenerTrait for TokioTcpListener {
    type TcpStream = tokio::net::TcpStream;

    async fn accept(&self) -> io::Result<(Self::TcpStream, String)> {
        let (stream, addr) = self.inner.accept().await?;
        Ok((stream, addr.to_string()))
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(self.inner.local_addr()?.to_string())
    }
}
