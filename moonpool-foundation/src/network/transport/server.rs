//! Event-driven server transport for handling multiple concurrent connections.
//!
//! This module provides an event-driven ServerTransport that can handle multiple
//! client connections simultaneously. Unlike the previous tick-based approach,
//! this implementation uses actors and channels for clean separation of concerns.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::network::transport::{Envelope, TransportError};
use crate::network::{NetworkProvider, TcpListenerTrait};
use crate::task::TaskProvider;
use crate::time::TimeProvider;

/// Unique identifier for each connection
pub type ConnectionId = u64;

/// Message received from any connection
#[derive(Debug)]
pub struct IncomingMessage<E> {
    /// The deserialized envelope
    pub envelope: E,
    /// Which connection this came from
    pub connection_id: ConnectionId,
    /// Peer address for logging/debugging
    pub peer_address: String,
}

/// Handle for managing a specific connection
struct ConnectionHandle {
    /// Channel to send data to this connection
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Peer address for reference
    #[allow(dead_code)]
    peer_address: String,
}

/// Event-driven server transport supporting multiple concurrent connections
///
/// This ServerTransport spawns background actors to handle connection acceptance
/// and per-connection I/O. All incoming messages from all connections flow through
/// a single channel, making it easy for higher-level components (like MessageCenter)
/// to process them.
pub struct ServerTransport<N, T, TP, E>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    E: Envelope + 'static,
{
    /// Network provider
    #[allow(dead_code)]
    network: N,

    /// Time provider
    #[allow(dead_code)]
    time: T,

    /// Task provider for spawning actors
    #[allow(dead_code)]
    task_provider: TP,

    /// Channel receiving all messages from all connections
    incoming_rx: mpsc::UnboundedReceiver<IncomingMessage<E>>,

    /// Map of active connections (Rc/RefCell for single-threaded efficiency)
    connections: Rc<RefCell<HashMap<ConnectionId, ConnectionHandle>>>,

    /// Bind address for reference
    bind_address: String,
}

impl<N, T, TP, E> ServerTransport<N, T, TP, E>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    E: Envelope + 'static,
{
    /// Create and start a new server transport
    ///
    /// This method binds to the specified address and spawns background actors
    /// to handle connection acceptance and I/O.
    pub async fn bind(
        network: N,
        time: T,
        task_provider: TP,
        address: &str,
    ) -> Result<Self, TransportError> {
        tracing::info!("ServerTransport binding to {}", address);

        // Bind to the address
        let listener = network.bind(address).await.map_err(|e| {
            TransportError::BindFailed(format!("Failed to bind to {}: {}", address, e))
        })?;

        // Create channels
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let connections = Rc::new(RefCell::new(HashMap::new()));

        // Spawn the accept loop actor
        let accept_connections = connections.clone();
        let accept_incoming_tx = incoming_tx.clone();
        let accept_task_provider = task_provider.clone();

        task_provider.spawn_task(
            "accept_loop",
            accept_loop::<N, TP, E>(
                listener,
                accept_incoming_tx,
                accept_connections,
                accept_task_provider,
            ),
        );

        tracing::info!("ServerTransport accept loop started for {}", address);

        Ok(Self {
            network,
            time,
            task_provider,
            incoming_rx,
            connections,
            bind_address: address.to_string(),
        })
    }

    /// Get the next message from any connection
    ///
    /// This method blocks until a message arrives from any active connection.
    /// It's the main way to receive messages in the event-driven model.
    pub async fn next_message(&mut self) -> Option<IncomingMessage<E>> {
        self.incoming_rx.recv().await
    }

    /// Send data to a specific connection
    pub fn send_to(
        &self,
        connection_id: ConnectionId,
        data: Vec<u8>,
    ) -> Result<(), TransportError> {
        self.connections
            .borrow()
            .get(&connection_id)
            .ok_or(TransportError::UnknownConnection)?
            .write_tx
            .send(data)
            .map_err(|_| TransportError::ConnectionClosed)
    }

    /// Send an envelope to a specific connection
    pub fn send_envelope(
        &self,
        connection_id: ConnectionId,
        envelope: &E,
    ) -> Result<(), TransportError> {
        let data = envelope.to_bytes();
        self.send_to(connection_id, data)
    }

    /// Reply to a received message (uses the connection from the message)
    pub fn reply(&self, msg: &IncomingMessage<E>, response: &E) -> Result<(), TransportError> {
        self.send_envelope(msg.connection_id, response)
    }

    /// Create and send a reply using the envelope's create_response method
    pub fn reply_with_payload(
        &self,
        request: &E,
        payload: Vec<u8>,
        msg: &IncomingMessage<E>,
    ) -> Result<(), TransportError> {
        let reply = request.create_response(payload);
        self.reply(msg, &reply)
    }

    /// Broadcast an envelope to all connected clients
    pub fn broadcast(&self, envelope: &E) -> Vec<(ConnectionId, Result<(), TransportError>)> {
        let data = envelope.to_bytes();
        self.connections
            .borrow()
            .iter()
            .map(|(id, handle)| {
                let result = handle
                    .write_tx
                    .send(data.clone())
                    .map_err(|_| TransportError::ConnectionClosed);
                (*id, result)
            })
            .collect()
    }

    /// Get the number of active connections
    pub fn connection_count(&self) -> usize {
        self.connections.borrow().len()
    }

    /// Check if there are any active connections
    pub fn has_connections(&self) -> bool {
        !self.connections.borrow().is_empty()
    }

    /// Get a list of all connection IDs
    pub fn connection_ids(&self) -> Vec<ConnectionId> {
        self.connections.borrow().keys().copied().collect()
    }

    /// Close a specific connection
    pub fn close_connection(&self, connection_id: ConnectionId) -> Result<(), TransportError> {
        self.connections
            .borrow_mut()
            .remove(&connection_id)
            .ok_or(TransportError::UnknownConnection)
            .map(|_| ())
    }

    /// Get the bind address
    pub fn bind_address(&self) -> &str {
        &self.bind_address
    }
}

/// Accept loop actor - handles incoming connections
async fn accept_loop<N, TP, E>(
    listener: N::TcpListener,
    incoming_tx: mpsc::UnboundedSender<IncomingMessage<E>>,
    connections: Rc<RefCell<HashMap<ConnectionId, ConnectionHandle>>>,
    task_provider: TP,
) where
    N: NetworkProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    E: Envelope + 'static,
{
    let mut next_connection_id = 0u64;

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                let connection_id = next_connection_id;
                next_connection_id += 1;

                tracing::info!("Accepted connection {} from {}", connection_id, peer_addr);

                // Create write channel for this connection
                let (write_tx, write_rx) = mpsc::unbounded_channel();

                // Store connection handle
                connections.borrow_mut().insert(
                    connection_id,
                    ConnectionHandle {
                        write_tx: write_tx.clone(),
                        peer_address: peer_addr.to_string(),
                    },
                );

                // Spawn connection handler actor
                let conn_incoming_tx = incoming_tx.clone();
                let conn_connections = connections.clone();

                task_provider.spawn_task(
                    &format!("connection_{}", connection_id),
                    handle_connection::<N::TcpStream, E>(
                        stream,
                        connection_id,
                        peer_addr.to_string(),
                        conn_incoming_tx,
                        write_rx,
                        conn_connections,
                    ),
                );
            }
            Err(e) => {
                tracing::error!("Accept error: {}", e);
                // Continue accepting despite errors
            }
        }
    }
}

/// Connection handler actor - manages I/O for a single connection
async fn handle_connection<TcpStream, E>(
    mut stream: TcpStream,
    connection_id: ConnectionId,
    peer_addr: String,
    incoming_tx: mpsc::UnboundedSender<IncomingMessage<E>>,
    mut write_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    connections: Rc<RefCell<HashMap<ConnectionId, ConnectionHandle>>>,
) where
    TcpStream: AsyncReadExt + AsyncWriteExt + Unpin,
    E: Envelope,
{
    let mut read_buffer = Vec::with_capacity(4096);

    tracing::debug!("Connection {} handler started", connection_id);

    loop {
        tokio::select! {
            // Read side - receive data and parse envelopes
            result = stream.read_buf(&mut read_buffer) => {
                match result {
                    Ok(0) => {
                        tracing::debug!("Connection {} closed by peer", connection_id);
                        break;
                    }
                    Ok(n) => {
                        tracing::trace!("Connection {} read {} bytes", connection_id, n);

                        // Try to parse complete envelopes from buffer using Envelope trait
                        loop {
                            match E::try_from_buffer(&mut read_buffer) {
                                Ok(Some(envelope)) => {
                                    let msg = IncomingMessage {
                                        envelope,
                                        connection_id,
                                        peer_address: peer_addr.clone(),
                                    };

                                    if incoming_tx.send(msg).is_err() {
                                        // Server has been dropped, exit
                                        tracing::debug!("Connection {}: server dropped, exiting", connection_id);
                                        return;
                                    }
                                }
                                Ok(None) => {
                                    // Buffer is empty, break the parsing loop
                                    break;
                                }
                                Err(crate::network::transport::types::EnvelopeError::InsufficientData { needed, available }) => {
                                    // Need more data - this is expected, break and continue reading
                                    tracing::trace!("Connection {}: need {} bytes, have {} bytes, waiting for more data",
                                        connection_id, needed, available);
                                    break;
                                }
                                Err(e) => {
                                    tracing::warn!("Connection {} parsing error: {}", connection_id, e);
                                    // For other errors, continue trying to parse remaining data
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Connection {} read error: {}", connection_id, e);
                        break;
                    }
                }
            }

            // Write side - send outgoing data
            Some(data) = write_rx.recv() => {
                if let Err(e) = stream.write_all(&data).await {
                    tracing::warn!("Connection {} write error: {}", connection_id, e);
                    break;
                }
                tracing::trace!("Connection {} sent {} bytes", connection_id, data.len());
            }
        }
    }

    // Cleanup when connection closes
    connections.borrow_mut().remove(&connection_id);
    tracing::debug!("Connection {} handler exiting", connection_id);
}

// We need to add the missing error type to TransportError
impl From<&str> for TransportError {
    fn from(msg: &str) -> Self {
        TransportError::IoError(msg.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_incoming_message_creation() {
        // Just test that we can create the types
        // Full integration tests will be in the ping-pong actors
    }

    #[test]
    fn test_connection_handle() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let handle = ConnectionHandle {
            write_tx: tx,
            peer_address: "127.0.0.1:1234".to_string(),
        };

        assert_eq!(handle.peer_address, "127.0.0.1:1234");
    }
}
