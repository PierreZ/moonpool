use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::network::transport::{Envelope, TransportProtocol};
use crate::network::{NetworkProvider, Peer, PeerConfig};
use crate::task::TaskProvider;
use crate::time::provider::TimeProvider;
use crate::{buggify_with_prob, sometimes_assert};

// Connection recovery constants following FoundationDB patterns
const INITIAL_RECONNECTION_TIME: Duration = Duration::from_millis(50);
const MAX_RECONNECTION_TIME: Duration = Duration::from_millis(500);
const RECONNECTION_TIME_GROWTH_RATE: f64 = 1.2;

/// State tracking for peer reconnection with exponential backoff
#[derive(Debug, Clone)]
struct PeerReconnectState {
    /// Current reconnection delay
    reconnect_delay: Duration,
    /// Time of last failure (for implementing delays)
    last_failure: Option<Instant>,
}

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
pub struct TransportDriver<N, T, TP, E>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    E: Envelope + 'static,
{
    /// Sans I/O protocol state machine
    protocol: TransportProtocol<E>,

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

    /// Reconnection state tracking for failed peers
    reconnect_states: HashMap<String, PeerReconnectState>,
}

impl<N, T, TP, E> TransportDriver<N, T, TP, E>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    E: Envelope + 'static,
{
    /// Create a new TransportDriver with the given components
    pub fn new(network: N, time: T, task_provider: TP, peer_config: PeerConfig) -> Self {
        Self {
            protocol: TransportProtocol::new(),
            peers: HashMap::new(),
            network,
            time,
            task_provider,
            peer_config,
            reconnect_states: HashMap::new(),
        }
    }

    /// Send an envelope to the specified destination
    ///
    /// This delegates to the protocol and then processes any pending transmissions.
    pub fn send(&mut self, destination: &str, envelope: E) -> Result<(), DriverError> {
        tracing::debug!(
            "TransportDriver::send called for destination: {}",
            destination
        );
        // Delegate to protocol (pure state transition)
        self.protocol.send(destination.to_string(), envelope);

        // Process transmissions (I/O operations)
        tracing::debug!("TransportDriver::send calling process_transmissions");
        self.process_transmissions()
    }

    /// Process all pending transmissions from the protocol
    ///
    /// This polls the protocol for transmissions and sends them via peers.
    pub fn process_transmissions(&mut self) -> Result<(), DriverError> {
        tracing::debug!("TransportDriver::process_transmissions called");
        while let Some(transmit) = self.protocol.poll_transmit() {
            tracing::debug!(
                "TransportDriver::process_transmissions got transmit for destination: {}, {} bytes",
                transmit.destination,
                transmit.data.len()
            );
            // Get or create peer for this destination
            let peer = self.get_or_create_peer(&transmit.destination)?;
            tracing::debug!("TransportDriver::process_transmissions got peer, calling peer.send()");

            // Buggify: Force send failure to test reconnection logic
            if buggify_with_prob!(0.05) {
                tracing::warn!("Buggify: Forcing peer send failure to test reconnection backoff");
                self.handle_peer_failure(&transmit.destination);

                sometimes_assert!(
                    driver_forced_send_failure,
                    true,
                    "Driver experiences forced send failures for testing reconnection"
                );

                return Err(DriverError::PeerOperationFailed(format!(
                    "Buggified send failure for {}",
                    transmit.destination
                )));
            }

            // Send data via peer
            match peer.borrow_mut().send(transmit.data) {
                Ok(_) => {
                    // Transmission successful
                    tracing::debug!("TransportDriver::process_transmissions peer.send() succeeded");
                }
                Err(peer_error) => {
                    tracing::debug!(
                        "TransportDriver::process_transmissions peer.send() failed: {:?}",
                        peer_error
                    );
                    // Handle peer failure with reconnection tracking
                    self.handle_peer_failure(&transmit.destination);

                    return Err(DriverError::PeerOperationFailed(format!(
                        "Failed to send to {}: {}",
                        transmit.destination, peer_error
                    )));
                }
            }
        }

        tracing::debug!("TransportDriver::process_transmissions completed");
        Ok(())
    }

    /// Process received data from all peers
    ///
    /// Polls all peers for received data and feeds it into the protocol.
    pub fn process_peer_reads(&mut self) {
        // Collect peer destinations first to avoid borrow checker issues
        let peer_destinations: Vec<String> = self.peers.keys().cloned().collect();

        for destination in peer_destinations {
            if let Some(peer_handle) = self.peers.get(&destination) {
                // Try to receive data from this peer
                if let Some(data) = peer_handle.borrow_mut().try_receive() {
                    // Feed received data into the protocol
                    self.protocol.handle_received(destination.clone(), data);
                }
            }
        }
    }

    /// Poll for the next received envelope from the protocol
    pub fn poll_receive(&mut self) -> Option<E> {
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

        // Check if we should attempt reconnection (respects exponential backoff)
        if !self.should_attempt_reconnect(destination) {
            return Err(DriverError::PeerCreationFailed(format!(
                "Reconnection backoff active for destination: {}",
                destination
            )));
        }

        // Buggify: Force peer creation failure to test backoff behavior
        if buggify_with_prob!(0.06) {
            tracing::warn!("Buggify: Forcing peer creation failure to test backoff");
            self.handle_peer_failure(destination);

            sometimes_assert!(
                driver_forced_peer_creation_failure,
                true,
                "Driver experiences forced peer creation failures for testing backoff"
            );

            return Err(DriverError::PeerCreationFailed(format!(
                "Buggified peer creation failure for {}",
                destination
            )));
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

        // Reset reconnection state on successful creation
        self.reset_reconnection_state(destination);

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

    /// Handle peer connection failure with exponential backoff
    fn handle_peer_failure(&mut self, destination: &str) {
        // Remove the failed peer from the pool
        self.peers.remove(destination);

        sometimes_assert!(
            driver_handles_peer_failure,
            true,
            "Driver handles peer connection failures"
        );

        // Update reconnection state with exponential backoff
        let state = self
            .reconnect_states
            .entry(destination.to_string())
            .or_insert(PeerReconnectState {
                reconnect_delay: INITIAL_RECONNECTION_TIME,
                last_failure: None,
            });

        let old_delay = state.reconnect_delay;

        // Record failure time and update delay for next attempt
        state.last_failure = Some(Instant::now());
        state.reconnect_delay = Duration::from_secs_f64(
            (state.reconnect_delay.as_secs_f64() * RECONNECTION_TIME_GROWTH_RATE)
                .min(MAX_RECONNECTION_TIME.as_secs_f64()),
        );

        // Track exponential backoff growth
        if state.reconnect_delay > old_delay {
            sometimes_assert!(
                driver_increases_backoff_delay,
                true,
                "Driver increases reconnection delay exponentially"
            );
        }

        // Track when delay reaches ceiling
        if state.reconnect_delay >= MAX_RECONNECTION_TIME {
            sometimes_assert!(
                driver_reaches_max_reconnection_delay,
                true,
                "Driver reaches maximum reconnection delay ceiling"
            );
        }
    }

    /// Check if we should attempt reconnection to a destination
    fn should_attempt_reconnect(&self, destination: &str) -> bool {
        if let Some(state) = self.reconnect_states.get(destination)
            && let Some(last_failure) = state.last_failure
        {
            let elapsed = last_failure.elapsed();
            let should_retry = elapsed >= state.reconnect_delay;

            // Track when backoff delay prevents reconnection
            if !should_retry {
                sometimes_assert!(
                    driver_backoff_prevents_reconnect,
                    true,
                    "Driver backoff delay prevents premature reconnection attempts"
                );
            } else {
                sometimes_assert!(
                    driver_allows_reconnect_after_backoff,
                    true,
                    "Driver allows reconnection after backoff period expires"
                );
            }

            // Check if enough time has passed since last failure
            return should_retry;
        }
        true // No previous failure recorded, can attempt connection
    }

    /// Reset reconnection state on successful connection
    fn reset_reconnection_state(&mut self, destination: &str) {
        let had_failure = self.reconnect_states.contains_key(destination);
        self.reconnect_states.remove(destination);

        // Track successful recovery after previous failures
        if had_failure {
            sometimes_assert!(
                driver_recovers_after_failure,
                true,
                "Driver successfully recovers connection after previous failures"
            );
        }
    }

    /// Get access to the time provider
    pub fn time(&self) -> &T {
        &self.time
    }

    /// Get access to the underlying protocol (for testing)
    pub fn protocol(&self) -> &TransportProtocol<E> {
        &self.protocol
    }

    /// Get mutable access to the underlying protocol (for testing)
    pub fn protocol_mut(&mut self) -> &mut TransportProtocol<E> {
        &mut self.protocol
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SimWorld;
    use crate::network::transport::Envelope;
    use crate::network::{RequestResponseEnvelope, SimNetworkProvider};
    use crate::task::tokio_provider::TokioTaskProvider;
    use crate::time::provider::TokioTimeProvider;

    fn create_test_driver() -> TransportDriver<
        SimNetworkProvider,
        TokioTimeProvider,
        TokioTaskProvider,
        RequestResponseEnvelope,
    > {
        let sim = SimWorld::new();
        let network = SimNetworkProvider::new(sim.downgrade());
        let time = TokioTimeProvider::new();
        let task_provider = TokioTaskProvider;
        let peer_config = PeerConfig::default();

        TransportDriver::new(network, time, task_provider, peer_config)
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
        let serialized = envelope.to_bytes();
        driver
            .protocol_mut()
            .handle_received("sender".to_string(), serialized);

        // Should now have received envelope
        let received = driver
            .poll_receive()
            .expect("Should have received envelope");
        assert_eq!(received.correlation_id(), 777);
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
