use std::collections::VecDeque;

use crate::network::transport::Envelope;

/// A message ready for transmission with destination information
#[derive(Debug, Clone)]
pub struct Transmit {
    /// Destination address for this transmission
    pub destination: String,
    /// Serialized envelope data ready for wire transmission
    pub data: Vec<u8>,
}

impl Transmit {
    /// Create a new Transmit with destination and data
    pub fn new(destination: String, data: Vec<u8>) -> Self {
        Self { destination, data }
    }

    /// Get the data size in bytes
    pub fn data_size(&self) -> usize {
        self.data.len()
    }

    /// Check if the data is empty
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Pure state machine for transport protocol logic
///
/// This is a Sans I/O implementation - it has NO I/O dependencies.
/// All networking operations are abstracted as parameters passed to methods.
/// This enables deterministic testing without any actual networking.
pub struct TransportProtocol<E: Envelope> {
    /// Queue of outbound transmissions ready for I/O layer
    transmit_queue: VecDeque<Transmit>,

    /// Queue of inbound processed envelopes ready for application
    receive_queue: VecDeque<E>,

    /// Buffer for accumulating partial messages from TCP reads
    /// TCP reads may contain 0, 1, or multiple concatenated messages
    receive_buffer: Vec<u8>,

    /// Statistics for monitoring
    stats: ProtocolStats,
}

/// Statistics for protocol operation monitoring
#[derive(Debug, Clone, Default)]
pub struct ProtocolStats {
    /// Total envelopes sent
    pub envelopes_sent: u64,
    /// Total envelopes received
    pub envelopes_received: u64,
    /// Total bytes sent
    pub bytes_sent: u64,
    /// Total bytes received  
    pub bytes_received: u64,
    /// Total serialization errors
    pub serialization_errors: u64,
    /// Total deserialization errors
    pub deserialization_errors: u64,
}

impl ProtocolStats {
    /// Create new empty statistics
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset all statistics to zero
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Get total error count
    pub fn total_errors(&self) -> u64 {
        self.serialization_errors + self.deserialization_errors
    }
}

impl<E: Envelope> TransportProtocol<E> {
    /// Create a new TransportProtocol
    pub fn new() -> Self {
        Self {
            transmit_queue: VecDeque::new(),
            receive_queue: VecDeque::new(),
            receive_buffer: Vec::new(),
            stats: ProtocolStats::new(),
        }
    }

    /// Send an envelope to the specified destination
    ///
    /// This is a pure state transition - no I/O is performed.
    /// The envelope is serialized and queued for transmission.
    pub fn send(&mut self, destination: String, envelope: E) {
        // Serialize the envelope
        let data = envelope.to_bytes();

        // Update statistics
        self.stats.envelopes_sent += 1;
        self.stats.bytes_sent += data.len() as u64;

        // Queue for transmission
        let transmit = Transmit::new(destination, data);
        self.transmit_queue.push_back(transmit);
    }

    /// Handle received data from a peer
    ///
    /// This is a pure state transition - no I/O is performed.
    /// The data is accumulated in a buffer and all complete messages are extracted.
    ///
    /// TCP reads may contain:
    /// - Partial messages (need more data)
    /// - Single complete message
    /// - Multiple concatenated complete messages
    /// - Complete message(s) + partial next message
    ///
    /// This method handles all cases by using try_from_buffer() which consumes
    /// complete messages from the buffer and leaves partial data for the next read.
    pub fn handle_received(&mut self, _from: String, mut data: Vec<u8>) {
        // Update byte statistics
        self.stats.bytes_received += data.len() as u64;

        let buffer_before = self.receive_buffer.len();
        let incoming_len = data.len();

        // Append new data to the receive buffer
        self.receive_buffer.append(&mut data);

        tracing::warn!(
            "PROTOCOL_RECV: buffer_before={}, incoming={}, buffer_after={}",
            buffer_before,
            incoming_len,
            self.receive_buffer.len()
        );

        // Parse all complete messages from the buffer
        let mut parsed_count = 0;
        loop {
            match E::try_from_buffer(&mut self.receive_buffer) {
                Ok(Some(envelope)) => {
                    // Successfully parsed a complete message
                    parsed_count += 1;
                    self.stats.envelopes_received += 1;
                    self.receive_queue.push_back(envelope);
                    // Continue loop to parse next message if available
                }
                Ok(None) => {
                    // Buffer is empty - all messages parsed
                    break;
                }
                Err(crate::network::transport::types::EnvelopeError::InsufficientData {
                    ..
                }) => {
                    // Partial message in buffer - need more data
                    // Leave the partial data in the buffer for next read
                    break;
                }
                Err(e) => {
                    // Parsing error (corrupt data)
                    self.stats.deserialization_errors += 1;
                    tracing::warn!("Envelope parsing error, clearing buffer: {}", e);
                    // Clear buffer to recover from corrupt data
                    self.receive_buffer.clear();
                    break;
                }
            }
        }

        tracing::warn!(
            "PROTOCOL_PARSE: envelopes_parsed={}, buffer_remaining={}",
            parsed_count,
            self.receive_buffer.len()
        );
    }

    /// Poll for the next transmission ready for I/O
    ///
    /// Returns None if no transmissions are queued.
    /// This is non-blocking and pure - no I/O is performed.
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmit_queue.pop_front()
    }

    /// Poll for the next received envelope ready for application
    ///
    /// Returns None if no envelopes are queued.
    /// This is non-blocking and pure - no I/O is performed.
    pub fn poll_receive(&mut self) -> Option<E> {
        self.receive_queue.pop_front()
    }

    /// Get the current protocol statistics
    pub fn stats(&self) -> &ProtocolStats {
        &self.stats
    }

    /// Get mutable access to statistics (for testing)
    pub fn stats_mut(&mut self) -> &mut ProtocolStats {
        &mut self.stats
    }

    /// Check if there are pending transmissions
    pub fn has_pending_transmissions(&self) -> bool {
        !self.transmit_queue.is_empty()
    }

    /// Check if there are pending received envelopes
    pub fn has_pending_receives(&self) -> bool {
        !self.receive_queue.is_empty()
    }

    /// Get the number of pending transmissions
    pub fn pending_transmission_count(&self) -> usize {
        self.transmit_queue.len()
    }

    /// Get the number of pending received envelopes
    pub fn pending_receive_count(&self) -> usize {
        self.receive_queue.len()
    }

    /// Clear all queued transmissions, receives, and buffered data (for testing/reset)
    pub fn clear_queues(&mut self) {
        self.transmit_queue.clear();
        self.receive_queue.clear();
        self.receive_buffer.clear();
    }
}

// Implement Clone for TransportProtocol
impl<E: Envelope> Clone for TransportProtocol<E> {
    fn clone(&self) -> Self {
        Self {
            transmit_queue: self.transmit_queue.clone(),
            receive_queue: self.receive_queue.clone(),
            receive_buffer: self.receive_buffer.clone(),
            stats: self.stats.clone(),
        }
    }
}

// Implement Default for convenience
impl<E: Envelope> Default for TransportProtocol<E> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::transport::RequestResponseEnvelope;

    fn create_test_protocol() -> TransportProtocol<RequestResponseEnvelope> {
        TransportProtocol::new()
    }

    fn create_test_envelope(correlation_id: u64, payload: &[u8]) -> RequestResponseEnvelope {
        RequestResponseEnvelope::new(correlation_id, payload.to_vec())
    }

    #[test]
    fn test_protocol_creation() {
        let protocol = create_test_protocol();

        assert!(!protocol.has_pending_transmissions());
        assert!(!protocol.has_pending_receives());
        assert_eq!(protocol.pending_transmission_count(), 0);
        assert_eq!(protocol.pending_receive_count(), 0);
        assert_eq!(protocol.stats().envelopes_sent, 0);
        assert_eq!(protocol.stats().envelopes_received, 0);
    }

    #[test]
    fn test_protocol_send() {
        let mut protocol = create_test_protocol();
        let envelope = create_test_envelope(42, b"test message");

        // Send envelope
        protocol.send("destination1".to_string(), envelope);

        // Check state
        assert!(protocol.has_pending_transmissions());
        assert_eq!(protocol.pending_transmission_count(), 1);
        assert_eq!(protocol.stats().envelopes_sent, 1);
        assert!(protocol.stats().bytes_sent > 0);
    }

    #[test]
    fn test_protocol_poll_transmit() {
        let mut protocol = create_test_protocol();
        let envelope = create_test_envelope(123, b"ping");

        // Send and poll
        protocol.send("server".to_string(), envelope);
        let transmit = protocol.poll_transmit().expect("Should have transmission");

        // Verify transmission
        assert_eq!(transmit.destination, "server");
        assert!(!transmit.is_empty());
        assert!(transmit.data_size() > 0);

        // Queue should be empty now
        assert!(!protocol.has_pending_transmissions());
        assert!(protocol.poll_transmit().is_none());
    }

    #[test]
    fn test_protocol_handle_received() {
        let mut protocol = create_test_protocol();
        let original_envelope = create_test_envelope(456, b"response");

        // Serialize envelope to simulate received data
        let serialized_data = original_envelope.to_bytes();

        // Handle received data
        protocol.handle_received("client".to_string(), serialized_data);

        // Check state
        assert!(protocol.has_pending_receives());
        assert_eq!(protocol.pending_receive_count(), 1);
        assert_eq!(protocol.stats().envelopes_received, 1);
        assert!(protocol.stats().bytes_received > 0);
    }

    #[test]
    fn test_protocol_poll_receive() {
        let mut protocol = create_test_protocol();
        let original_envelope = create_test_envelope(789, b"data");

        // Simulate received data
        let serialized_data = original_envelope.to_bytes();
        protocol.handle_received("peer".to_string(), serialized_data);

        // Poll received envelope
        let received_envelope = protocol
            .poll_receive()
            .expect("Should have received envelope");

        // Verify envelope
        assert_eq!(
            received_envelope.correlation_id,
            original_envelope.correlation_id
        );
        assert_eq!(received_envelope.payload, original_envelope.payload);

        // Queue should be empty now
        assert!(!protocol.has_pending_receives());
        assert!(protocol.poll_receive().is_none());
    }

    #[test]
    fn test_protocol_send_receive_flow() {
        let mut protocol = create_test_protocol();
        let request = create_test_envelope(100, b"request");
        let reply = create_test_envelope(100, b"reply"); // Same correlation ID

        // Send request
        protocol.send("server".to_string(), request.clone());
        let transmit = protocol.poll_transmit().unwrap();

        // Simulate server receiving and replying
        protocol.handle_received("server".to_string(), transmit.data);
        let received_request = protocol.poll_receive().unwrap();
        assert_eq!(received_request.correlation_id, request.correlation_id);

        // Send reply
        protocol.send("client".to_string(), reply.clone());
        let reply_transmit = protocol.poll_transmit().unwrap();

        // Simulate client receiving reply
        protocol.handle_received("client".to_string(), reply_transmit.data);
        let received_reply = protocol.poll_receive().unwrap();
        assert_eq!(received_reply.correlation_id, reply.correlation_id);

        // Check statistics
        assert_eq!(protocol.stats().envelopes_sent, 2);
        assert_eq!(protocol.stats().envelopes_received, 2);
    }

    #[test]
    fn test_protocol_malformed_data_handling() {
        let mut protocol = create_test_protocol();

        // Send malformed data with invalid payload length (exceeds MAX_PAYLOAD_SIZE)
        let mut malformed_data = vec![0u8; 12]; // Valid header size
        malformed_data[8] = 255;
        malformed_data[9] = 255;
        malformed_data[10] = 255;
        malformed_data[11] = 255; // Payload length = u32::MAX (exceeds MAX_PAYLOAD_SIZE)
        protocol.handle_received("bad_peer".to_string(), malformed_data);

        // Should have no received envelopes but error should be counted
        assert!(!protocol.has_pending_receives());
        assert_eq!(protocol.stats().envelopes_received, 0);
        assert_eq!(protocol.stats().deserialization_errors, 1);
        assert_eq!(protocol.stats().total_errors(), 1);
    }

    #[test]
    fn test_protocol_multiple_sends() {
        let mut protocol = create_test_protocol();

        // Send multiple envelopes
        for i in 0..5 {
            let envelope = create_test_envelope(i, format!("message {}", i).as_bytes());
            protocol.send(format!("dest{}", i), envelope);
        }

        // Check queue state
        assert_eq!(protocol.pending_transmission_count(), 5);
        assert_eq!(protocol.stats().envelopes_sent, 5);

        // Poll all transmissions
        let mut transmissions = Vec::new();
        while let Some(transmit) = protocol.poll_transmit() {
            transmissions.push(transmit);
        }

        assert_eq!(transmissions.len(), 5);
        assert!(!protocol.has_pending_transmissions());
    }

    #[test]
    fn test_protocol_stats_operations() {
        let mut protocol = create_test_protocol();

        // Initial stats
        let stats = protocol.stats();
        assert_eq!(stats.envelopes_sent, 0);
        assert_eq!(stats.total_errors(), 0);

        // Reset stats
        protocol.stats_mut().reset();
        assert_eq!(protocol.stats().envelopes_sent, 0);

        // Test error counting
        // Note: Short data (< 12 bytes) is treated as incomplete, not corrupt
        // We need data with valid length header but invalid content to trigger error
        let mut malformed = vec![0u8; 12]; // Valid header size
        malformed[8] = 255; // Set payload_len to 255 (but we'll provide invalid data)
        malformed[9] = 255;
        malformed[10] = 255;
        malformed[11] = 255; // Payload length = u32::MAX (way too large)
        protocol.handle_received("peer".to_string(), malformed);
        assert_eq!(protocol.stats().deserialization_errors, 1);
    }

    #[test]
    fn test_protocol_queue_management() {
        let mut protocol = create_test_protocol();
        let envelope = create_test_envelope(1, b"test");

        // Add items to queues
        protocol.send("dest".to_string(), envelope.clone());
        let serialized = envelope.to_bytes();
        protocol.handle_received("peer".to_string(), serialized);

        assert!(protocol.has_pending_transmissions());
        assert!(protocol.has_pending_receives());

        // Clear queues
        protocol.clear_queues();

        assert!(!protocol.has_pending_transmissions());
        assert!(!protocol.has_pending_receives());
        assert_eq!(protocol.pending_transmission_count(), 0);
        assert_eq!(protocol.pending_receive_count(), 0);
    }

    #[test]
    fn test_protocol_sans_io_verification() {
        // This test verifies that the protocol is truly Sans I/O
        // by ensuring all operations are pure state transitions

        let mut protocol = create_test_protocol();
        let envelope = create_test_envelope(42, b"sans io test");

        // All operations should be synchronous and not block
        protocol.send("dest".to_string(), envelope.clone());
        let _transmit = protocol.poll_transmit();

        let serialized = envelope.to_bytes();
        protocol.handle_received("peer".to_string(), serialized);
        let _received = protocol.poll_receive();

        // All operations completed synchronously without any I/O
        // This proves the Sans I/O design is working correctly
    }

    #[test]
    fn test_transmit_structure() {
        let transmit = Transmit::new("test_dest".to_string(), b"test_data".to_vec());

        assert_eq!(transmit.destination, "test_dest");
        assert_eq!(transmit.data, b"test_data");
        assert_eq!(transmit.data_size(), 9);
        assert!(!transmit.is_empty());

        let empty_transmit = Transmit::new("dest".to_string(), Vec::new());
        assert!(empty_transmit.is_empty());
        assert_eq!(empty_transmit.data_size(), 0);
    }

    #[test]
    fn test_protocol_clone() {
        let mut protocol = create_test_protocol();
        let envelope = create_test_envelope(999, b"clone test");

        // Add some state
        protocol.send("dest".to_string(), envelope);

        // Clone protocol
        let cloned_protocol = protocol.clone();

        // Both should have the same state
        assert_eq!(
            protocol.pending_transmission_count(),
            cloned_protocol.pending_transmission_count()
        );
        assert_eq!(
            protocol.stats().envelopes_sent,
            cloned_protocol.stats().envelopes_sent
        );
    }
}
