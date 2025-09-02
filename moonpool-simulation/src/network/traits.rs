use async_trait::async_trait;
use std::io;
use tokio::io::{AsyncRead, AsyncWrite};

/// Provider trait for creating network connections and listeners
/// Single-core design - no Send bounds needed
/// Clone allows sharing providers across multiple peers efficiently
#[async_trait(?Send)]
pub trait NetworkProvider: Clone {
    /// The TCP stream type for this provider
    type TcpStream: AsyncRead + AsyncWrite + Unpin + 'static;
    /// The TCP listener type for this provider
    type TcpListener: TcpListenerTrait<TcpStream = Self::TcpStream> + 'static;

    /// Create a TCP listener bound to the given address
    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener>;

    /// Connect to a remote address  
    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream>;
}

/// Trait for TCP listeners that can accept connections
#[async_trait(?Send)]
pub trait TcpListenerTrait {
    /// The TCP stream type that this listener produces
    type TcpStream: AsyncRead + AsyncWrite + Unpin + 'static;

    /// Accept a single incoming connection
    async fn accept(&self) -> io::Result<(Self::TcpStream, String)>;

    /// Get the local address this listener is bound to
    fn local_addr(&self) -> io::Result<String>;
}
