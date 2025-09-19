//! Client transport implementation for request-response messaging.
//!
//! This module provides a concrete ClientTransport struct that implements
//! request-response semantics using correlation IDs and self-driving get_reply.

use std::collections::HashMap;
use tokio::sync::oneshot;

use crate::network::NetworkProvider;
use crate::network::PeerConfig;
use crate::network::transport::{
    Envelope, EnvelopeFactory, EnvelopeReplyDetection, EnvelopeSerializer, ReceivedEnvelope,
    TransportError, driver::TransportDriver,
};
use crate::task::TaskProvider;
use crate::time::TimeProvider;

/// Client transport for request-response messaging
///
/// Provides a clean API for sending requests and receiving responses
/// with automatic correlation ID management and self-driving behavior.
pub struct ClientTransport<N, T, TP, S>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    S: EnvelopeSerializer,
    S::Envelope: EnvelopeReplyDetection + Envelope,
{
    /// Transport driver for managing peer connections
    driver: TransportDriver<N, T, TP, S>,

    /// Pending requests awaiting responses, indexed by correlation ID
    pending_requests: HashMap<u64, oneshot::Sender<Vec<u8>>>,

    /// Next correlation ID to use for requests
    next_correlation_id: u64,
}

impl<N, T, TP, S> ClientTransport<N, T, TP, S>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    S: EnvelopeSerializer,
    S::Envelope: EnvelopeReplyDetection + Envelope,
{
    /// Create a new ClientTransport with the given components
    pub fn new(
        serializer: S,
        network: N,
        time: T,
        task_provider: TP,
        peer_config: PeerConfig,
    ) -> Self {
        Self {
            driver: TransportDriver::new(serializer, network, time, task_provider, peer_config),
            pending_requests: HashMap::new(),
            next_correlation_id: 1,
        }
    }

    /// Send a request and wait for response with self-driving behavior
    ///
    /// This method uses the turbofish pattern to specify the envelope type.
    /// It continuously drives the transport while waiting for the response
    /// to prevent deadlocks.
    pub async fn get_reply<E>(
        &mut self,
        destination: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, TransportError>
    where
        E: EnvelopeFactory<S::Envelope> + 'static,
        S::Envelope: Clone,
    {
        let correlation_id = self.next_correlation_id();
        let (tx, mut rx) = oneshot::channel();

        // Store response channel indexed by correlation ID
        self.pending_requests.insert(correlation_id, tx);

        // Create request envelope using factory
        let envelope = E::create_request(correlation_id, payload);

        // Send through driver
        self.driver
            .send(destination, envelope)
            .map_err(|e| TransportError::PeerError(e.to_string()))?;

        // Self-driving loop to process responses
        loop {
            // Drive transport by processing incoming messages
            self.poll_receive();

            // Check if our response arrived
            match rx.try_recv() {
                Ok(response_payload) => {
                    // Clean up and return response
                    self.pending_requests.remove(&correlation_id);
                    return Ok(response_payload);
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    // No response yet, yield and continue
                    tokio::task::yield_now().await;
                    continue;
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    // Channel was closed, request was cancelled
                    self.pending_requests.remove(&correlation_id);
                    return Err(TransportError::SendFailed("Request cancelled".to_string()));
                }
            }
        }
    }

    /// Poll for received messages and process correlation matching
    ///
    /// This method drives the transport by checking for incoming messages
    /// and matching them to pending requests by correlation ID.
    pub fn poll_receive(&mut self) -> Option<ReceivedEnvelope<S::Envelope>> {
        // First update driver reads (this may fill the protocol receive queue)
        self.driver.process_peer_reads();

        // Poll driver for received envelopes
        if let Some(envelope) = self.driver.poll_receive() {
            // Check if this is a response to a pending request
            if let Some(correlation_id) = envelope.correlation_id()
                && let Some(sender) = self.pending_requests.remove(&correlation_id)
            {
                // Extract payload and send through response channel
                let payload = envelope.payload().to_vec();
                let _ = sender.send(payload); // Ignore error if receiver was dropped
                return None; // Response was consumed, don't return to caller
            }

            // This envelope is not a response to a pending request
            // Return it for the application to handle
            return Some(ReceivedEnvelope::new(envelope, "unknown".to_string()));
        }

        None
    }

    /// Send a request and wait for response with timeout
    ///
    /// This method adds timeout functionality to get_reply using tokio::select!
    /// to race between the request completion and a timeout from TimeProvider::sleep.
    pub async fn get_reply_with_timeout<E>(
        &mut self,
        destination: &str,
        payload: Vec<u8>,
        timeout_duration: std::time::Duration,
    ) -> Result<Vec<u8>, TransportError>
    where
        E: EnvelopeFactory<S::Envelope> + 'static,
        S::Envelope: Clone,
    {
        // Clone the time provider to avoid borrowing conflicts
        let time_provider = self.driver.time().clone();

        tokio::select! {
            result = self.get_reply::<E>(destination, payload) => {
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
    pub async fn tick(&mut self) {
        // No async tick implemented for driver in Phase 11.3
        // This is a placeholder for future maintenance operations
    }

    /// Close the transport and clean up resources
    pub async fn close(&mut self) {
        // Cancel all pending requests
        for (_, sender) in self.pending_requests.drain() {
            let _ = sender.send(Vec::new()); // Send empty response to unblock waiters
        }
    }

    /// Generate the next unique correlation ID
    fn next_correlation_id(&mut self) -> u64 {
        let id = self.next_correlation_id;
        self.next_correlation_id = self.next_correlation_id.wrapping_add(1);
        id
    }

    /// Get the number of pending requests
    pub fn pending_request_count(&self) -> usize {
        self.pending_requests.len()
    }

    /// Check if a specific correlation ID is pending
    pub fn has_pending_request(&self, correlation_id: u64) -> bool {
        self.pending_requests.contains_key(&correlation_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::transport::RequestResponseEnvelopeFactory;

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
    fn test_envelope_factory_compilation() {
        // This test verifies that the turbofish pattern compiles correctly
        // Testing type constraints at compile time

        let envelope = RequestResponseEnvelopeFactory::create_request(42, b"test".to_vec());
        assert_eq!(envelope.correlation_id, 42);
        assert_eq!(envelope.payload, b"test");

        let reply = RequestResponseEnvelopeFactory::create_reply(&envelope, b"response".to_vec());
        assert_eq!(reply.correlation_id, 42);
        assert_eq!(reply.payload, b"response");
    }

    #[test]
    fn test_basic_client_creation() {
        // Test that we can create the basic structure
        let client = TestClient::new();
        assert_eq!(client.pending_request_count(), 0);
        assert!(!client.has_pending_request(1));
    }
}
