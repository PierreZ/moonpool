//! Simplified network transport using foundation's unified Envelope trait.
//!
//! This module provides a clean NetworkTransport trait and implementation
//! using foundation's transport layer directly with Message as the envelope type.

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

/// Simple wrapper that implements NetworkTransport using foundation's ClientTransport.
///
/// With the new envelope-only API, this is much simpler: Message implements Envelope,
/// so we use ClientTransport<N, T, TP, Message> directly without any serializer.
pub struct FoundationTransport<N, T, TP>
where
    N: moonpool_foundation::network::NetworkProvider + Clone + 'static,
    T: moonpool_foundation::time::TimeProvider + Clone + 'static,
    TP: moonpool_foundation::task::TaskProvider + Clone + 'static,
{
    /// Foundation's ClientTransport with Message as the envelope type
    transport: moonpool_foundation::network::transport::ClientTransport<N, T, TP, Message>,
}

impl<N, T, TP> FoundationTransport<N, T, TP>
where
    N: moonpool_foundation::network::NetworkProvider + Clone + 'static,
    T: moonpool_foundation::time::TimeProvider + Clone + 'static,
    TP: moonpool_foundation::task::TaskProvider + Clone + 'static,
{
    /// Create a new FoundationTransport.
    ///
    /// With the envelope-only API, we just need to pass the providers directly.
    /// Message implements Envelope, so ClientTransport<N, T, TP, Message> handles everything.
    pub fn new(
        network: N,
        time: T,
        task_provider: TP,
        peer_config: moonpool_foundation::network::PeerConfig,
    ) -> Self {
        Self {
            transport: moonpool_foundation::network::transport::ClientTransport::new(
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
        // Direct usage: Message implements Envelope, so just send it
        // No double serialization! Foundation handles Message.to_bytes() internally
        self.transport
            .send(destination, message)
            .await
            .map_err(|e| ActorError::ProcessingFailed(format!("Network send failed: {}", e)))
    }

    fn poll_receive(&self) -> Option<Message> {
        // Poll for received messages - the envelope IS the Message
        self.transport
            .poll_receive()
            .map(|received| received.envelope)
    }
}
