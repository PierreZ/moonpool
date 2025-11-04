//! Client transport implementation for request-response messaging.
//!
//! This module provides a concrete ClientTransport struct that implements
//! request-response semantics using correlation IDs and self-driving get_reply.

use std::cell::RefCell;
use std::collections::HashMap;
use tokio::sync::oneshot;

use crate::network::NetworkProvider;
use crate::network::PeerConfig;
use crate::network::transport::{
    Envelope, ReceivedEnvelope, TransportError, driver::TransportDriver,
};
use crate::task::TaskProvider;
use crate::time::TimeProvider;

/// Client transport for request-response messaging
///
/// Provides a clean API for sending requests and receiving responses
/// with automatic correlation ID management and self-driving behavior.
///
/// Uses interior mutability (Cell/RefCell) to allow `&self` methods,
/// enabling safe shared usage in single-threaded async contexts.
pub struct ClientTransport<N, T, TP, E>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    E: Envelope + 'static,
{
    /// Transport driver for managing peer connections
    /// Uses RefCell for interior mutability
    driver: RefCell<TransportDriver<N, T, TP, E>>,

    /// Pending requests awaiting responses, indexed by correlation ID
    /// Uses RefCell for interior mutability in single-threaded context
    pending_requests: RefCell<HashMap<u64, oneshot::Sender<Vec<u8>>>>,
}

impl<N, T, TP, E> ClientTransport<N, T, TP, E>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    E: Envelope + 'static,
{
    /// Create a new ClientTransport with the given components
    pub fn new(network: N, time: T, task_provider: TP, peer_config: PeerConfig) -> Self {
        Self {
            driver: RefCell::new(TransportDriver::new(
                network,
                time,
                task_provider,
                peer_config,
            )),
            pending_requests: RefCell::new(HashMap::new()),
        }
    }

    /// Send a request envelope and wait for response
    ///
    /// This method sends an envelope directly to the destination and waits for a response.
    /// It continuously drives the transport while waiting to prevent deadlocks.
    ///
    /// # Arguments
    ///
    /// * `destination` - Target node address (e.g., "127.0.0.1:8001")
    /// * `envelope` - Request envelope to send
    ///
    /// # Returns
    ///
    /// Response envelope of the same type
    pub async fn send(&self, destination: &str, envelope: E) -> Result<E, TransportError> {
        tracing::debug!(
            "ClientTransport::send called for destination: {}",
            destination
        );

        let correlation_id = envelope.correlation_id();
        let (tx, mut rx) = oneshot::channel();
        tracing::debug!(
            "ClientTransport::send using correlation_id: {}",
            correlation_id
        );

        // Store response channel indexed by correlation ID
        self.pending_requests
            .borrow_mut()
            .insert(correlation_id, tx);

        // Send envelope directly through driver
        self.driver
            .borrow_mut()
            .send(destination, envelope)
            .map_err(|e| {
                tracing::debug!("ClientTransport::send driver.send() failed: {:?}", e);
                TransportError::PeerError(e.to_string())
            })?;

        tracing::debug!(
            "ClientTransport::send driver.send() succeeded, entering self-driving loop"
        );

        // Self-driving loop to process responses
        loop {
            // Drive transport by processing incoming messages
            self.poll_receive();

            // Check if our response arrived
            match rx.try_recv() {
                Ok(response_payload) => {
                    // Deserialize response payload back to envelope type
                    let response = E::from_bytes(&response_payload).map_err(|e| {
                        TransportError::SendFailed(format!("Failed to deserialize response: {}", e))
                    })?;

                    // Clean up and return response
                    self.pending_requests.borrow_mut().remove(&correlation_id);
                    return Ok(response);
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    // No response yet, yield and continue
                    tokio::task::yield_now().await;
                    continue;
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    // Channel was closed, request was cancelled
                    self.pending_requests.borrow_mut().remove(&correlation_id);
                    return Err(TransportError::SendFailed("Request cancelled".to_string()));
                }
            }
        }
    }

    /// Poll for received messages and process correlation matching
    ///
    /// This method drives the transport by checking for incoming messages
    /// and matching them to pending requests by correlation ID.
    ///
    /// Uses `&self` with RefCell to ensure compatibility with `&self` methods.
    pub fn poll_receive(&self) -> Option<ReceivedEnvelope<E>> {
        // Borrow driver for processing (released after this block)
        {
            let mut driver = self.driver.borrow_mut();
            // First update driver reads (this may fill the protocol receive queue)
            driver.process_peer_reads();
        }

        // Poll driver for received envelopes (separate borrow)
        let envelope = self.driver.borrow_mut().poll_receive();

        if let Some(envelope) = envelope {
            // Check if this is a response to a pending request
            let correlation_id = Envelope::correlation_id(&envelope);
            {
                // Borrow and check/remove in a single scope
                let sender = self.pending_requests.borrow_mut().remove(&correlation_id);

                if let Some(sender) = sender {
                    // Extract payload and send through response channel
                    let payload = Envelope::payload(&envelope).to_vec();
                    let _ = sender.send(payload); // Ignore error if receiver was dropped
                    return None; // Response was consumed, don't return to caller
                }
            }

            // This envelope is not a response to a pending request
            // Return it for the application to handle
            return Some(ReceivedEnvelope::new(envelope, "unknown".to_string()));
        }

        None
    }

    /// Send a request envelope and wait for response with timeout
    ///
    /// This method adds timeout functionality using tokio::select!
    /// to race between the request completion and a timeout from TimeProvider::sleep.
    pub async fn send_with_timeout(
        &self,
        destination: &str,
        envelope: E,
        timeout_duration: std::time::Duration,
    ) -> Result<E, TransportError> {
        // Clone the time provider to avoid borrowing conflicts
        let time_provider = self.driver.borrow().time().clone();

        tokio::select! {
            result = self.send(destination, envelope) => {
                result
            }
            _ = time_provider.sleep(timeout_duration) => {
                Err(TransportError::Timeout)
            }
        }
    }

    /// Periodic maintenance operations
    ///
    /// Should be called regularly to perform driver maintenance.
    pub async fn tick(&self) {
        // No async tick implemented for driver in Phase 11.3
        // This is a placeholder for future maintenance operations
    }

    /// Close the transport and clean up resources
    pub async fn close(&self) {
        // Cancel all pending requests
        // Take ownership of all pending requests via drain
        let pending = self
            .pending_requests
            .borrow_mut()
            .drain()
            .collect::<Vec<_>>();
        for (_, sender) in pending {
            let _ = sender.send(Vec::new()); // Send empty response to unblock waiters
        }
    }

    /// Get the number of pending requests
    pub fn pending_request_count(&self) -> usize {
        self.pending_requests.borrow().len()
    }

    /// Check if a specific correlation ID is pending
    pub fn has_pending_request(&self, correlation_id: u64) -> bool {
        self.pending_requests.borrow().contains_key(&correlation_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Simplified test struct for testing correlation ID generation
    struct TestClient {
        pending_requests: HashMap<u64, oneshot::Sender<Vec<u8>>>,
        next_correlation_id: u64,
    }

    impl TestClient {
        fn new() -> Self {
            Self {
                pending_requests: HashMap::new(),
                next_correlation_id: 1,
            }
        }

        fn next_correlation_id(&mut self) -> u64 {
            let id = self.next_correlation_id;
            self.next_correlation_id = self.next_correlation_id.wrapping_add(1);
            id
        }

        fn pending_request_count(&self) -> usize {
            self.pending_requests.len()
        }

        fn has_pending_request(&self, correlation_id: u64) -> bool {
            self.pending_requests.contains_key(&correlation_id)
        }
    }

    #[test]
    fn test_correlation_id_generation() {
        let mut client = TestClient::new();

        let id1 = client.next_correlation_id();
        let id2 = client.next_correlation_id();
        let id3 = client.next_correlation_id();

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_correlation_id_wrapping() {
        let mut client = TestClient::new();
        client.next_correlation_id = u64::MAX;

        let id1 = client.next_correlation_id();
        let id2 = client.next_correlation_id();

        assert_eq!(id1, u64::MAX);
        assert_eq!(id2, 0);
    }

    #[test]
    fn test_pending_request_management() {
        let mut client = TestClient::new();
        assert_eq!(client.pending_request_count(), 0);

        let (tx, _rx) = oneshot::channel();
        client.pending_requests.insert(42, tx);

        assert_eq!(client.pending_request_count(), 1);
        assert!(client.has_pending_request(42));
        assert!(!client.has_pending_request(43));
    }

    #[test]
    fn test_basic_client_creation() {
        // Test that we can create the basic structure
        let client = TestClient::new();
        assert_eq!(client.pending_request_count(), 0);
        assert!(!client.has_pending_request(1));
    }
}
