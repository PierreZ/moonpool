use async_trait::async_trait;
use std::collections::VecDeque;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{
    EnvelopeFactory, EnvelopeReplyDetection, EnvelopeSerializer, NetTransport, ReceivedEnvelope,
    TransportDriver, TransportError,
};
use crate::network::{NetworkProvider, TcpListenerTrait};
use crate::task::TaskProvider;
use crate::time::TimeProvider;

/// Server-side transport implementation that can accept incoming connections.
///
/// This transport wraps a TransportDriver and adds server-specific functionality
/// like binding to an address and accepting incoming connections. It integrates
/// with the existing peer infrastructure through the driver.
pub struct ServerTransport<N, T, TP, S>
where
    N: NetworkProvider,
    T: TimeProvider,
    TP: TaskProvider,
    S: EnvelopeSerializer,
{
    /// Underlying transport driver for I/O operations
    driver: TransportDriver<N, T, TP, S>,
    /// TCP listener for accepting connections (when bound)
    listener: Option<N::TcpListener>,
    /// Address we're bound to (for reference)
    bind_address: Option<String>,
    /// Accepted client connection stream
    client_stream: Option<(N::TcpStream, String)>,
    /// Pending writes to be sent to client
    pending_writes: VecDeque<Vec<u8>>,
}

impl<N, T, TP, S> ServerTransport<N, T, TP, S>
where
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
    S: EnvelopeSerializer,
{
    /// Create a new server transport
    pub fn new(serializer: S, network: N, time: T, task_provider: TP) -> Self {
        let driver = TransportDriver::new(serializer, network, time, task_provider);

        Self {
            driver,
            listener: None,
            bind_address: None,
            client_stream: None,
            pending_writes: VecDeque::new(),
        }
    }

    /// Check if the transport is currently bound to an address
    pub fn is_bound(&self) -> bool {
        self.listener.is_some()
    }

    /// Get the address we're bound to, if any
    pub fn bind_address(&self) -> Option<&str> {
        self.bind_address.as_deref()
    }

    /// Get statistics about the underlying driver
    pub fn stats(&self) -> super::TransportDriverStats {
        self.driver.queue_stats()
    }
}

#[async_trait(?Send)]
impl<N, T, TP, S> NetTransport<S> for ServerTransport<N, T, TP, S>
where
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
    S: EnvelopeSerializer,
    S::Envelope: EnvelopeReplyDetection + EnvelopeFactory<S>,
{
    async fn bind(&mut self, address: &str) -> Result<(), TransportError> {
        // Get network provider from driver to create listener
        let listener = self.driver.network.bind(address).await.map_err(|e| {
            TransportError::BindFailed(format!("Failed to bind to {}: {:?}", address, e))
        })?;

        self.listener = Some(listener);
        self.bind_address = Some(address.to_string());

        tracing::info!("ServerTransport: Bound to address {}", address);
        Ok(())
    }

    async fn get_reply<E>(
        &mut self,
        _destination: &str,
        _payload: Vec<u8>,
    ) -> Result<Vec<u8>, TransportError>
    where
        E: EnvelopeFactory<S> + EnvelopeReplyDetection + 'static,
    {
        // Servers typically don't initiate requests, but we implement it for completeness
        // This would be used if a server needs to make requests to other servers
        Err(TransportError::SendFailed(
            "Server get_reply not implemented yet".to_string(),
        ))
    }

    fn send_reply(&mut self, request: &S::Envelope, payload: Vec<u8>) -> Result<(), TransportError>
    where
        S::Envelope: EnvelopeFactory<S>,
    {
        // Create reply envelope using the factory
        let reply_envelope = S::Envelope::create_reply(request, payload);

        // For servers, we should send responses back through the existing client connection
        // instead of creating a new outbound connection
        if let Some((ref mut _stream, ref peer_addr)) = self.client_stream {
            // Serialize the envelope and send it directly through the stream
            let data = self.driver.serialize_envelope(&reply_envelope);
            // Queue the data to be written during the next tick
            self.pending_writes.push_back(data);
            tracing::debug!("ServerTransport: Queued reply for client {}", peer_addr);
        } else {
            tracing::warn!("ServerTransport: No client connection to send reply to");
        }
        Ok(())
    }

    fn poll_receive(&mut self) -> Option<ReceivedEnvelope<S::Envelope>> {
        self.driver.poll_receive().map(|envelope| ReceivedEnvelope {
            from: "client".to_string(), // Simplified for Phase 11
            envelope,
        })
    }

    async fn tick(&mut self) {
        // Accept a connection if we don't have one yet
        if self.client_stream.is_none()
            && let Some(ref listener) = self.listener
        {
            // Just try to accept - this will work if a connection is available
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    tracing::debug!("ServerTransport: Accepted connection from {}", peer_addr);
                    self.client_stream = Some((stream, peer_addr));
                }
                Err(e) => {
                    tracing::debug!("ServerTransport: No connection to accept: {:?}", e);
                }
            }

            // Write any pending responses to client FIRST (before reading more data)
            if let Some((ref mut stream, ref peer_addr)) = self.client_stream {
                while let Some(data) = self.pending_writes.pop_front() {
                    tracing::debug!(
                        "ServerTransport: Writing {} bytes to client {}",
                        data.len(),
                        peer_addr
                    );
                    match stream.write_all(&data).await {
                        Ok(_) => {
                            tracing::debug!(
                                "ServerTransport: Successfully wrote {} bytes to client {}",
                                data.len(),
                                peer_addr
                            );
                        }
                        Err(e) => {
                            tracing::debug!(
                                "ServerTransport: Write error to {}: {:?}",
                                peer_addr,
                                e
                            );
                            self.client_stream = None;
                            break;
                        }
                    }
                }
            }

            // Read data from client if we have a connection
            if let Some((ref mut stream, ref peer_addr)) = self.client_stream {
                let mut buffer = [0u8; 4096];
                tracing::trace!(
                    "ServerTransport: Attempting to read from connection {}",
                    peer_addr
                );
                match stream.read(&mut buffer).await {
                    Ok(0) => {
                        // Connection closed
                        tracing::debug!("ServerTransport: Client {} disconnected", peer_addr);
                        self.client_stream = None;
                    }
                    Ok(n) => {
                        let data = buffer[..n].to_vec();
                        tracing::debug!("ServerTransport: Read {} bytes from {}", n, peer_addr);
                        // Forward to transport protocol
                        self.driver.on_peer_received(peer_addr.clone(), data);
                    }
                    Err(e) => {
                        tracing::debug!("ServerTransport: Read error from {}: {:?}", peer_addr, e);
                        self.client_stream = None;
                    }
                }
            } else {
                tracing::trace!("ServerTransport: No client connection to read from");
            }

            // Drive the underlying transport
            self.driver.tick().await;
        }
    }

    async fn close(&mut self) {
        tracing::debug!("ServerTransport: Closing connections");

        // Flush any pending writes before closing
        if let Some((ref mut stream, ref peer_addr)) = self.client_stream {
            while let Some(data) = self.pending_writes.pop_front() {
                tracing::debug!(
                    "ServerTransport: Flushing {} bytes to client {} before close",
                    data.len(),
                    peer_addr
                );
                match stream.write_all(&data).await {
                    Ok(_) => {
                        tracing::debug!(
                            "ServerTransport: Successfully flushed {} bytes to client {}",
                            data.len(),
                            peer_addr
                        );
                    }
                    Err(e) => {
                        tracing::debug!(
                            "ServerTransport: Write error during flush to {}: {:?}",
                            peer_addr,
                            e
                        );
                        break;
                    }
                }
            }
        }

        // Stop listening for new connections
        if self.listener.is_some() {
            tracing::debug!(
                "ServerTransport: Stopped listening on {}",
                self.bind_address.as_deref().unwrap_or("unknown")
            );
            self.listener = None;
        }

        // Close existing client connection
        if let Some((_, peer_addr)) = &self.client_stream {
            tracing::debug!(
                "ServerTransport: Closing client connection to {}",
                peer_addr
            );
            self.client_stream = None;
        }

        self.driver.close().await;
    }
}

// TODO: Add integration tests once provider creation is properly set up
