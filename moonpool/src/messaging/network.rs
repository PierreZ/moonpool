//! Simplified network transport using foundation's unified Envelope trait.
//!
//! This module provides type aliases and simplified interfaces for using
//! foundation's transport layer directly with Actor messages.

use crate::error::ActorError;
use crate::messaging::Message;
use async_trait::async_trait;

/// Simplified trait for sending actor messages over the network.
///
/// Now that Message implements Envelope trait, we can use it directly
/// with foundation's transport without complex wrappers.
#[async_trait(?Send)]
pub trait NetworkTransport {
    /// Send an actor message to a destination address.
    ///
    /// Takes `&self` instead of `&mut self` to allow calling from RefCell borrows.
    ///
    /// # Parameters
    ///
    /// - `destination`: Target node address (e.g., "127.0.0.1:8001")
    /// - `message`: Actor message to send
    ///
    /// # Returns
    ///
    /// - `Ok(response_message)`: Response message from the remote node
    /// - `Err(ActorError)`: Network error occurred
    async fn send(&self, destination: &str, message: Message) -> Result<Message, ActorError>;

    /// Poll for incoming messages from the network.
    ///
    /// Takes `&self` to allow interior mutability pattern.
    ///
    /// # Returns
    ///
    /// - `Some(message)`: Received actor message
    /// - `None`: No messages available
    fn poll_receive(&self) -> Option<Message>;
}

/// Serializer that uses Message as the envelope type directly.
///
/// This serializer allows Message to be the transport's envelope type,
/// enabling zero-copy usage of the unified Envelope trait.
#[derive(Clone)]
pub struct MessageSerializer;

impl moonpool_foundation::network::transport::EnvelopeSerializer for MessageSerializer {
    type Envelope = Message;

    fn serialize(&self, envelope: &Self::Envelope) -> Vec<u8> {
        use moonpool_foundation::network::transport::Envelope;
        envelope.to_bytes()
    }

    fn deserialize(
        &self,
        data: &[u8],
    ) -> Result<Self::Envelope, moonpool_foundation::network::transport::EnvelopeError> {
        use moonpool_foundation::network::transport::Envelope;
        Message::from_bytes(data)
    }

    fn try_deserialize_from_buffer(
        &self,
        buffer: &mut Vec<u8>,
    ) -> Result<Option<Self::Envelope>, moonpool_foundation::network::transport::EnvelopeError>
    {
        use moonpool_foundation::network::transport::Envelope;
        Message::try_from_buffer(buffer)
    }
}

/// Type alias for foundation's ClientTransport using Message directly.
///
/// This demonstrates the improved DX: Message is the envelope type,
/// enabling direct usage without double serialization.
pub type ActorTransport<N, T, TP> =
    moonpool_foundation::network::transport::ClientTransport<N, T, TP, MessageSerializer>;

// Legacy trait implementations needed for foundation's server transport
impl moonpool_foundation::network::transport::EnvelopeReplyDetection for Message {
    fn is_reply_to(&self, correlation_id: u64) -> bool {
        self.correlation_id.as_u64() == correlation_id
            && self.direction == crate::messaging::Direction::Response
    }

    fn correlation_id(&self) -> Option<u64> {
        Some(self.correlation_id.as_u64())
    }
}

/// Factory for creating Message instances for server replies
pub struct MessageEnvelopeFactory;

impl moonpool_foundation::network::transport::EnvelopeFactory<Message> for MessageEnvelopeFactory {
    fn create_request(correlation_id: u64, payload: Vec<u8>) -> Message {
        // Create a basic request message - real usage should provide proper actor IDs
        Message {
            correlation_id: crate::actor::CorrelationId::new(correlation_id),
            direction: crate::messaging::Direction::Request,
            target_actor: crate::actor::ActorId::new("system", "unknown", "unknown"),
            sender_actor: crate::actor::ActorId::new("system", "client", "default"),
            target_node: crate::actor::NodeId::from("127.0.0.1:8000").unwrap(),
            sender_node: crate::actor::NodeId::from("127.0.0.1:8001").unwrap(),
            method_name: "request".to_string(),
            payload,
            flags: crate::messaging::MessageFlags::empty(),
            time_to_expiry: None,
            forward_count: 0,
            cache_invalidation: None,
        }
    }

    fn create_reply(request: &Message, payload: Vec<u8>) -> Message {
        Message::response(request, payload)
    }

    fn extract_payload(envelope: &Message) -> &[u8] {
        &envelope.payload
    }
}

/// Simple wrapper that implements NetworkTransport using foundation's transport directly.
///
/// This demonstrates the improved DX: much simpler than the previous 110-line wrapper!
pub struct FoundationTransport<N, T, TP>
where
    N: moonpool_foundation::network::NetworkProvider + Clone + 'static,
    T: moonpool_foundation::time::TimeProvider + Clone + 'static,
    TP: moonpool_foundation::task::TaskProvider + Clone + 'static,
{
    transport: ActorTransport<N, T, TP>,
}

impl<N, T, TP> FoundationTransport<N, T, TP>
where
    N: moonpool_foundation::network::NetworkProvider + Clone + 'static,
    T: moonpool_foundation::time::TimeProvider + Clone + 'static,
    TP: moonpool_foundation::task::TaskProvider + Clone + 'static,
{
    /// Create a new FoundationTransport using foundation's ClientTransport directly.
    pub fn new(
        network: N,
        time: T,
        task_provider: TP,
        peer_config: moonpool_foundation::network::PeerConfig,
    ) -> Self {
        Self {
            transport: moonpool_foundation::network::transport::ClientTransport::new(
                MessageSerializer,
                network,
                time,
                task_provider,
                peer_config,
            ),
        }
    }
}

#[async_trait(?Send)]
impl<N, T, TP> NetworkTransport for FoundationTransport<N, T, TP>
where
    N: moonpool_foundation::network::NetworkProvider + Clone + 'static,
    T: moonpool_foundation::time::TimeProvider + Clone + 'static,
    TP: moonpool_foundation::task::TaskProvider + Clone + 'static,
{
    async fn send(&self, destination: &str, message: Message) -> Result<Message, ActorError> {
        // With MessageSerializer, the transport envelope IS the Message.
        // We use the standard request() method which will:
        // 1. Create a Message envelope using MessageEnvelopeFactory::create_request()
        // 2. Serialize it using MessageSerializer::serialize() -> Message::to_bytes()
        // 3. Send over wire
        // 4. Receive response and deserialize using MessageSerializer::deserialize()
        //
        // The "payload" we pass becomes the payload field of the created Message envelope.
        use moonpool_foundation::network::transport::Envelope;
        let payload = message.to_bytes();

        let response_bytes = self
            .transport
            .request::<MessageEnvelopeFactory>(destination, payload)
            .await
            .map_err(|e| ActorError::ProcessingFailed(format!("Network send failed: {}", e)))?;

        // response_bytes is the payload from the received Message envelope
        let response = Message::from_bytes(&response_bytes).map_err(|e| {
            ActorError::ProcessingFailed(format!("Failed to deserialize response: {}", e))
        })?;

        Ok(response)
    }

    fn poll_receive(&self) -> Option<Message> {
        // Poll for received messages - envelope IS the Message now
        self.transport.poll_receive().map(|received| {
            // The envelope is already a Message, just return it
            received.envelope
        })
    }
}
