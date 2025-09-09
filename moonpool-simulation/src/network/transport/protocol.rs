use std::collections::VecDeque;
use std::time::Instant;

use super::{EnvelopeSerializer, Transmit};

/// Pure Sans I/O protocol state machine for transport layer.
///
/// This state machine contains no I/O dependencies and handles all protocol logic
/// in a deterministic, testable manner. It operates by:
/// - Accepting commands via method calls
/// - Maintaining internal state  
/// - Producing outputs that the I/O driver processes
/// - Accepting time as a parameter for deterministic testing
///
/// The I/O driver is responsible for bridging between this state machine
/// and actual network operations.
pub struct TransportProtocol<S: EnvelopeSerializer> {
    /// Envelope serialization strategy
    serializer: S,
    /// Counter for generating unique tokens
    next_token: u64,

    /// Queue of messages ready to be transmitted by I/O driver
    transmit_queue: VecDeque<Transmit>,
    /// Queue of received envelopes ready for application consumption
    receive_queue: VecDeque<S::Envelope>,
}

impl<S: EnvelopeSerializer> TransportProtocol<S> {
    /// Create a new transport protocol instance
    pub fn new(serializer: S) -> Self {
        Self {
            serializer,
            next_token: 1,
            transmit_queue: VecDeque::new(),
            receive_queue: VecDeque::new(),
        }
    }

    /// Generate next unique token for envelope correlation
    fn next_token(&mut self) -> u64 {
        let token = self.next_token;
        self.next_token += 1;
        token
    }

    /// Get the current token counter value (for testing/debugging)
    pub fn current_token(&self) -> u64 {
        self.next_token
    }

    /// Send envelope to destination.
    ///
    /// This is a pure state transition that queues the envelope for transmission.
    /// The I/O driver will poll for queued transmissions and perform actual network I/O.
    pub fn send(&mut self, destination: String, envelope: S::Envelope) {
        let data = self.serializer.serialize(&envelope);
        self.transmit_queue
            .push_back(Transmit::new(destination, data));
    }

    /// Handle received data from the network (pure state transition).
    ///
    /// Deserializes the data and queues the envelope for application consumption.
    /// Invalid data is logged but does not affect protocol state.
    pub fn handle_received(&mut self, from: String, data: Vec<u8>) {
        match self.serializer.deserialize(&data) {
            Ok(envelope) => {
                tracing::debug!("Transport: Received envelope from {}", from);
                self.receive_queue.push_back(envelope);
            }
            Err(e) => {
                tracing::warn!("Transport: Failed to deserialize from {}: {:?}", from, e);
            }
        }
    }

    /// Poll for queued transmissions (non-blocking).
    ///
    /// The I/O driver calls this to get messages that need to be transmitted.
    /// Returns None when no transmissions are queued.
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmit_queue.pop_front()
    }

    /// Poll for received envelopes (non-blocking).
    ///
    /// Applications call this to get messages that have been received and processed.
    /// Returns None when no messages are available.
    pub fn poll_receive(&mut self) -> Option<S::Envelope> {
        self.receive_queue.pop_front()
    }

    /// Handle timeouts and periodic maintenance (time passed as parameter - Sans I/O).
    ///
    /// This method allows the protocol to perform time-based operations without
    /// depending on system time, enabling deterministic testing.
    ///
    /// Future enhancements might include:
    /// - Connection timeout tracking
    /// - Retry logic
    /// - Cleanup of stale state
    pub fn handle_timeout(&mut self, _now: Instant) {
        // Future: timeout tracking, cleanup, retry logic, etc.
    }

    /// Get the number of queued transmissions (for testing/monitoring)
    pub fn transmit_queue_len(&self) -> usize {
        self.transmit_queue.len()
    }

    /// Get the number of queued received messages (for testing/monitoring)
    pub fn receive_queue_len(&self) -> usize {
        self.receive_queue.len()
    }

    /// Serialize an envelope using the protocol's serializer
    pub fn serialize_envelope(&self, envelope: &S::Envelope) -> Vec<u8> {
        self.serializer.serialize(envelope)
    }

    /// Clear all queued state (for testing)
    #[cfg(test)]
    pub fn clear_queues(&mut self) {
        self.transmit_queue.clear();
        self.receive_queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::transport::{RequestResponseEnvelope, RequestResponseSerializer};

    #[test]
    fn test_protocol_send_queues_transmission() {
        let mut protocol = TransportProtocol::new(RequestResponseSerializer);

        let envelope = RequestResponseEnvelope {
            destination_token: 42,
            source_token: 1,
            payload: b"test".to_vec(),
        };

        protocol.send("127.0.0.1:8080".to_string(), envelope);

        assert_eq!(protocol.transmit_queue_len(), 1);
        assert_eq!(protocol.receive_queue_len(), 0);
    }

    #[test]
    fn test_protocol_poll_transmit() {
        let mut protocol = TransportProtocol::new(RequestResponseSerializer);

        let envelope = RequestResponseEnvelope {
            destination_token: 42,
            source_token: 1,
            payload: b"test".to_vec(),
        };

        protocol.send("127.0.0.1:8080".to_string(), envelope);

        let transmit = protocol.poll_transmit().expect("should have transmission");
        assert_eq!(transmit.destination, "127.0.0.1:8080");
        assert!(transmit.data.len() > 0);

        assert_eq!(protocol.transmit_queue_len(), 0);
        assert!(protocol.poll_transmit().is_none());
    }

    #[test]
    fn test_protocol_handle_received() {
        let mut protocol = TransportProtocol::new(RequestResponseSerializer);
        let serializer = RequestResponseSerializer;

        let envelope = RequestResponseEnvelope {
            destination_token: 42,
            source_token: 1,
            payload: b"test".to_vec(),
        };

        let data = serializer.serialize(&envelope);
        protocol.handle_received("127.0.0.1:8080".to_string(), data);

        assert_eq!(protocol.receive_queue_len(), 1);

        let received = protocol
            .poll_receive()
            .expect("should have received message");
        assert_eq!(received.destination_token, 42);
        assert_eq!(received.source_token, 1);
        assert_eq!(received.payload, b"test");
    }

    #[test]
    fn test_protocol_handle_received_invalid_data() {
        let mut protocol = TransportProtocol::new(RequestResponseSerializer);

        // Send invalid data (too short)
        protocol.handle_received("127.0.0.1:8080".to_string(), vec![1, 2, 3]);

        // Should not crash, should not queue any messages
        assert_eq!(protocol.receive_queue_len(), 0);
    }

    #[test]
    fn test_protocol_token_generation() {
        let mut protocol = TransportProtocol::new(RequestResponseSerializer);

        assert_eq!(protocol.current_token(), 1);

        let token1 = protocol.next_token();
        assert_eq!(token1, 1);
        assert_eq!(protocol.current_token(), 2);

        let token2 = protocol.next_token();
        assert_eq!(token2, 2);
        assert_eq!(protocol.current_token(), 3);
    }
}
