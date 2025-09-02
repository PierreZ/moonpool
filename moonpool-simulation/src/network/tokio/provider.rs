use crate::network::traits::{NetworkProvider, TcpListenerTrait};
use async_trait::async_trait;
use std::io;
use tracing::instrument;

/// Real Tokio networking implementation
#[derive(Debug, Clone)]
pub struct TokioNetworkProvider;

impl TokioNetworkProvider {
    /// Create a new Tokio network provider
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

    #[instrument(skip(self))]
    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Ok(TokioTcpListener { inner: listener })
    }

    #[instrument(skip(self))]
    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream> {
        tokio::net::TcpStream::connect(addr).await
    }
}

/// Wrapper for Tokio TcpListener to implement our trait
#[derive(Debug)]
pub struct TokioTcpListener {
    inner: tokio::net::TcpListener,
}

#[async_trait(?Send)]
impl TcpListenerTrait for TokioTcpListener {
    type TcpStream = tokio::net::TcpStream;

    #[instrument(skip(self))]
    async fn accept(&self) -> io::Result<(Self::TcpStream, String)> {
        let (stream, addr) = self.inner.accept().await?;
        Ok((stream, addr.to_string()))
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(self.inner.local_addr()?.to_string())
    }
}
