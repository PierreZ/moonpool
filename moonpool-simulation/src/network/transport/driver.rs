use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::{EnvelopeSerializer, TransportProtocol};
use crate::network::{NetworkProvider, Peer, PeerConfig};
use crate::task::TaskProvider;
use crate::time::TimeProvider;

/// I/O driver that bridges the Sans I/O transport protocol to actual network operations.
///
/// The driver integrates the pure protocol state machine with the existing Peer
/// infrastructure to provide actual network I/O capabilities. It maintains a pool
/// of peer connections and routes messages between the protocol and peers.
///
/// Architecture:
/// - TransportProtocol<S>: Pure state machine (no I/O)
/// - TransportDriver: I/O integration layer (this struct)
/// - Peer: Actual network connection management
pub struct TransportDriver<N, T, TP, S>
where
    N: NetworkProvider,
    T: TimeProvider,
    TP: TaskProvider,
    S: EnvelopeSerializer,
{
    /// Sans I/O protocol state machine
    protocol: TransportProtocol<S>,

    /// Pool of peer connections by destination address
    peers: HashMap<String, Rc<RefCell<Peer<N, T, TP>>>>,

    /// Network provider for creating new connections
    pub(super) network: N,
    /// Time provider for timeouts and delays
    pub(super) time: T,
    /// Task provider for spawning background actors
    pub(super) task_provider: TP,

    /// Default configuration for new peers
    default_peer_config: PeerConfig,
}

impl<N, T, TP, S> TransportDriver<N, T, TP, S>
where
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
    S: EnvelopeSerializer,
{
    /// Create a new transport driver
    pub fn new(serializer: S, network: N, time: T, task_provider: TP) -> Self {
        Self {
            protocol: TransportProtocol::new(serializer),
            peers: HashMap::new(),
            network,
            time,
            task_provider,
            default_peer_config: PeerConfig::default(),
        }
    }

    /// Create a new transport driver with custom peer configuration
    pub fn new_with_config(
        serializer: S,
        network: N,
        time: T,
        task_provider: TP,
        peer_config: PeerConfig,
    ) -> Self {
        Self {
            protocol: TransportProtocol::new(serializer),
            peers: HashMap::new(),
            network,
            time,
            task_provider,
            default_peer_config: peer_config,
        }
    }

    /// Send envelope to destination.
    ///
    /// This delegates to the Sans I/O protocol for pure state management,
    /// then processes any queued transmissions through actual I/O.
    pub fn send(&mut self, destination: &str, envelope: S::Envelope) {
        // Delegate to Sans I/O protocol (pure state transition)
        self.protocol.send(destination.to_string(), envelope);

        // Process queued transmissions through I/O layer
        self.process_transmissions();
    }

    /// Get or create a peer connection for the destination
    fn get_or_create_peer(&mut self, destination: &str) -> Rc<RefCell<Peer<N, T, TP>>> {
        if !self.peers.contains_key(destination) {
            let peer = Peer::new(
                self.network.clone(),
                self.time.clone(),
                self.task_provider.clone(),
                destination.to_string(),
                self.default_peer_config.clone(),
            );
            self.peers
                .insert(destination.to_string(), Rc::new(RefCell::new(peer)));
        }
        self.peers[destination].clone()
    }

    /// Process transmissions from the protocol state machine to actual I/O
    fn process_transmissions(&mut self) {
        while let Some(transmit) = self.protocol.poll_transmit() {
            let peer = self.get_or_create_peer(&transmit.destination);

            // Try to send through the peer (non-blocking)
            if let Ok(mut peer_ref) = peer.try_borrow_mut() {
                match peer_ref.send(transmit.data) {
                    Ok(_) => {
                        tracing::debug!("Transport: Sent message to {}", transmit.destination);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Transport: Failed to send to {}: {:?}",
                            transmit.destination,
                            e
                        );
                    }
                }
            } else {
                tracing::warn!(
                    "Transport: Could not acquire peer lock for {}",
                    transmit.destination
                );
                // In a more sophisticated implementation, we might re-queue this transmission
            }
        }
    }

    /// Handle data received from a peer connection.
    ///
    /// This should be called by the peer when it receives data, bridging
    /// from I/O back to the protocol state machine.
    pub fn on_peer_received(&mut self, from: String, data: Vec<u8>) {
        self.protocol.handle_received(from, data);
    }

    /// Poll for received envelopes from the protocol
    pub fn poll_receive(&mut self) -> Option<S::Envelope> {
        self.protocol.poll_receive()
    }

    /// Serialize an envelope using the protocol's serializer
    pub fn serialize_envelope(&self, envelope: &S::Envelope) -> Vec<u8> {
        self.protocol.serialize_envelope(envelope)
    }

    /// Drive the transport layer (should be called periodically).
    ///
    /// This performs periodic maintenance:
    /// - Processes queued transmissions
    /// - Handles protocol timeouts
    /// - Reads data from peer connections
    /// - Updates metrics
    pub async fn tick(&mut self) {
        let now = self.time.now();

        // Let protocol handle time-based operations
        self.protocol.handle_timeout(now);

        // Process any queued transmissions
        self.process_transmissions();

        // Read from all peer connections (non-blocking)
        self.process_peer_reads();
    }

    /// Read data from all peer connections and forward to protocol (non-blocking)
    fn process_peer_reads(&mut self) {
        // Collect destinations to avoid borrowing issues
        let destinations: Vec<String> = self.peers.keys().cloned().collect();

        tracing::trace!(
            "TransportDriver: Processing reads from {} peers",
            destinations.len()
        );
        for destination in destinations {
            if let Some(peer_rc) = self.peers.get(&destination) {
                if let Ok(mut peer) = peer_rc.try_borrow_mut() {
                    // Try to receive data (non-blocking)
                    let mut received_count = 0;
                    while let Some(data) = peer.try_receive() {
                        received_count += 1;
                        tracing::debug!(
                            "TransportDriver: Received {} bytes from {}",
                            data.len(),
                            destination
                        );
                        self.protocol.handle_received(destination.clone(), data);
                    }
                    if received_count == 0 {
                        tracing::trace!(
                            "TransportDriver: No data available from peer {}",
                            destination
                        );
                    }
                } else {
                    tracing::trace!("TransportDriver: Could not borrow peer {}", destination);
                }
            }
        }
    }

    /// Get the number of active peer connections
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Get statistics about queue sizes
    pub fn queue_stats(&self) -> TransportDriverStats {
        let transmit_queue_len = self.protocol.transmit_queue_len();
        let receive_queue_len = self.protocol.receive_queue_len();

        let total_peer_queue_size = self
            .peers
            .values()
            .filter_map(|peer| peer.try_borrow().ok())
            .map(|peer| peer.queue_size())
            .sum();

        TransportDriverStats {
            transmit_queue_len,
            receive_queue_len,
            peer_count: self.peers.len(),
            total_peer_queue_size,
        }
    }

    /// Close all peer connections and clean up resources
    pub async fn close(&mut self) {
        for (destination, peer_rc) in self.peers.drain() {
            if let Ok(mut peer) = peer_rc.try_borrow_mut() {
                tracing::debug!("Transport: Closing peer connection to {}", destination);
                peer.close().await;
            }
        }
    }
}

/// Statistics about the transport driver state
#[derive(Debug, Clone)]
pub struct TransportDriverStats {
    /// Number of messages queued for transmission
    pub transmit_queue_len: usize,
    /// Number of received messages awaiting consumption  
    pub receive_queue_len: usize,
    /// Number of active peer connections
    pub peer_count: usize,
    /// Total messages queued across all peers
    pub total_peer_queue_size: usize,
}

// TODO: Add integration tests once provider creation is properly set up
