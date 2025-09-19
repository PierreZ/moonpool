use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::network::transport::{EnvelopeSerializer, TransportProtocol};
use crate::network::{NetworkProvider, Peer, PeerConfig};
use crate::task::TaskProvider;
use crate::time::provider::TimeProvider;

/// Errors that can occur during transport driver operations
#[derive(Debug, Clone)]
pub enum DriverError {
    /// Failed to create a new peer connection
    PeerCreationFailed(String),
    /// Peer operation failed
    PeerOperationFailed(String),
    /// No peer available for destination
    NoPeerAvailable(String),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::PeerCreationFailed(msg) => write!(f, "Peer creation failed: {}", msg),
            DriverError::PeerOperationFailed(msg) => write!(f, "Peer operation failed: {}", msg),
            DriverError::NoPeerAvailable(dest) => {
                write!(f, "No peer available for destination: {}", dest)
            }
        }
    }
}

impl std::error::Error for DriverError {}

/// Type alias for peer handles wrapped in Rc<RefCell<>>
type PeerHandle<N, T, TP> = Rc<RefCell<Peer<N, T, TP>>>;

/// Transport driver that bridges Sans I/O protocol to actual Peer connections
///
/// This component connects the pure TransportProtocol state machine to Phase 10 Peers
/// for actual network I/O operations. It manages a pool of peer connections and drives
/// the protocol by processing transmissions and receives.
pub struct TransportDriver<N, T, TP, S>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    S: EnvelopeSerializer,
{
    /// Sans I/O protocol state machine
    protocol: TransportProtocol<S>,

    /// Pool of peer connections indexed by destination
    peers: HashMap<String, PeerHandle<N, T, TP>>,

    /// Network provider for creating connections
    network: N,

    /// Time provider for timeouts and delays
    time: T,

    /// Task provider for spawning background tasks
    task_provider: TP,

    /// Configuration for new peer connections
    peer_config: PeerConfig,
}

impl<N, T, TP, S> TransportDriver<N, T, TP, S>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    S: EnvelopeSerializer,
{
    /// Create a new TransportDriver with the given components
    pub fn new(
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
            peer_config,
        }
    }

    /// Send an envelope to the specified destination
    ///
    /// This delegates to the protocol and then processes any pending transmissions.
    pub fn send(&mut self, destination: &str, envelope: S::Envelope) -> Result<(), DriverError> {
        // Delegate to protocol (pure state transition)
        self.protocol.send(destination.to_string(), envelope);

        // Process transmissions (I/O operations)
        self.process_transmissions()
    }

    /// Process all pending transmissions from the protocol
    ///
    /// This polls the protocol for transmissions and sends them via peers.
    pub fn process_transmissions(&mut self) -> Result<(), DriverError> {
        while let Some(transmit) = self.protocol.poll_transmit() {
            // Get or create peer for this destination
            let peer = self.get_or_create_peer(&transmit.destination)?;

            // Send data via peer
            match peer.borrow_mut().send(transmit.data) {
                Ok(_) => {
                    // Transmission successful
                }
                Err(peer_error) => {
                    return Err(DriverError::PeerOperationFailed(format!(
                        "Failed to send to {}: {}",
                        transmit.destination, peer_error
                    )));
                }
            }
        }

        Ok(())
    }

    /// Process received data from all peers
    ///
    /// This is a placeholder for future implementation that will need async polling.
    /// For Phase 11.3, we focus on basic send functionality.
    pub fn process_peer_reads(&mut self) {
        // TODO: Implement non-blocking receive polling when Peer API supports it
        // For now, this is a no-op to maintain the driver interface
    }

    /// Poll for the next received envelope from the protocol
    pub fn poll_receive(&mut self) -> Option<S::Envelope> {
        self.protocol.poll_receive()
    }

    /// Get or create a peer for the specified destination
    fn get_or_create_peer(
        &mut self,
        destination: &str,
    ) -> Result<PeerHandle<N, T, TP>, DriverError> {
        // Check if we already have a peer for this destination
        if let Some(peer) = self.peers.get(destination) {
            return Ok(peer.clone());
        }

        // Create new peer (returns Self, not Result)
        let peer = Peer::new(
            self.network.clone(),
            self.time.clone(),
            self.task_provider.clone(),
            destination.to_string(),
            self.peer_config.clone(),
        );

        let peer_handle = Rc::new(RefCell::new(peer));
        self.peers
            .insert(destination.to_string(), peer_handle.clone());
        Ok(peer_handle)
    }

    /// Get the number of active peer connections
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Check if a peer exists for the given destination
    pub fn has_peer(&self, destination: &str) -> bool {
        self.peers.contains_key(destination)
    }

    /// Get the protocol statistics
    pub fn stats(&self) -> &crate::network::transport::ProtocolStats {
        self.protocol.stats()
    }

    /// Get a list of all peer destinations
    pub fn peer_destinations(&self) -> Vec<String> {
        self.peers.keys().cloned().collect()
    }

    /// Remove a peer connection (for cleanup or error recovery)
    pub fn remove_peer(&mut self, destination: &str) -> bool {
        self.peers.remove(destination).is_some()
    }

    /// Clear all peer connections
    pub fn clear_peers(&mut self) {
        self.peers.clear();
    }

    /// Periodic maintenance for the driver
    ///
    /// This processes all pending I/O operations and should be called regularly.
    pub async fn tick(&mut self) -> Result<(), DriverError> {
        // Process any pending transmissions
        self.process_transmissions()?;

        // Process received data from all peers
        self.process_peer_reads();

        Ok(())
    }

    /// Get access to the underlying protocol (for testing)
    pub fn protocol(&self) -> &TransportProtocol<S> {
        &self.protocol
    }

    /// Get mutable access to the underlying protocol (for testing)
    pub fn protocol_mut(&mut self) -> &mut TransportProtocol<S> {
        &mut self.protocol
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SimWorld;
    use crate::network::transport::{Envelope, EnvelopeReplyDetection};
    use crate::network::{RequestResponseEnvelope, RequestResponseSerializer, SimNetworkProvider};
    use crate::task::tokio_provider::TokioTaskProvider;
    use crate::time::provider::TokioTimeProvider;

    fn create_test_driver() -> TransportDriver<
        SimNetworkProvider,
        TokioTimeProvider,
        TokioTaskProvider,
        RequestResponseSerializer,
    > {
        let serializer = RequestResponseSerializer::new();
        let sim = SimWorld::new();
        let network = SimNetworkProvider::new(sim.downgrade());
        let time = TokioTimeProvider::new();
        let task_provider = TokioTaskProvider;
        let peer_config = PeerConfig::default();

        TransportDriver::new(serializer, network, time, task_provider, peer_config)
    }

    fn create_test_envelope(correlation_id: u64, payload: &[u8]) -> RequestResponseEnvelope {
        RequestResponseEnvelope::new(correlation_id, payload.to_vec())
    }

    #[test]
    fn test_driver_creation() {
        let driver = create_test_driver();

        assert_eq!(driver.peer_count(), 0);
        assert!(driver.peer_destinations().is_empty());
        assert_eq!(driver.stats().envelopes_sent, 0);
        assert_eq!(driver.stats().envelopes_received, 0);
    }

    #[test]
    fn test_driver_protocol_integration() {
        let mut driver = create_test_driver();
        let envelope = create_test_envelope(42, b"test message");

        // Initially no messages in protocol
        assert_eq!(driver.stats().envelopes_sent, 0);

        // Send message directly to protocol to test integration
        driver
            .protocol_mut()
            .send("destination1".to_string(), envelope);

        // Protocol should have the message
        assert_eq!(driver.protocol().stats().envelopes_sent, 1);
        assert!(driver.protocol().has_pending_transmissions());
    }

    #[test]
    fn test_driver_poll_receive() {
        let mut driver = create_test_driver();

        // Initially no received envelopes
        assert!(driver.poll_receive().is_none());

        // Simulate received data by feeding it directly to protocol
        let envelope = create_test_envelope(777, b"received data");
        let serialized = driver.protocol().serializer().serialize(&envelope);
        driver
            .protocol_mut()
            .handle_received("sender".to_string(), serialized);

        // Should now have received envelope
        let received = driver
            .poll_receive()
            .expect("Should have received envelope");
        assert_eq!(EnvelopeReplyDetection::correlation_id(&received), Some(777));
        assert_eq!(received.payload(), b"received data");

        // No more envelopes
        assert!(driver.poll_receive().is_none());
    }

    #[test]
    fn test_driver_error_display() {
        let peer_creation_error = DriverError::PeerCreationFailed("test error".to_string());
        let peer_operation_error = DriverError::PeerOperationFailed("operation failed".to_string());
        let no_peer_error = DriverError::NoPeerAvailable("missing_dest".to_string());

        assert!(
            peer_creation_error
                .to_string()
                .contains("Peer creation failed")
        );
        assert!(
            peer_operation_error
                .to_string()
                .contains("Peer operation failed")
        );
        assert!(no_peer_error.to_string().contains("No peer available"));
    }
}
