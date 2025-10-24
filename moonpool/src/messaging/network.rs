//! Network transport abstraction for MessageBus.
//!
//! This module provides a trait-based abstraction over the foundation's transport
//! layer, allowing MessageBus to use network communication without being generic
//! over provider types.

use crate::error::ActorError;
use async_trait::async_trait;

/// Trait for sending messages over the network.
///
/// This abstracts over foundation's ClientTransport to avoid making MessageBus
/// generic over NetworkProvider, TimeProvider, and TaskProvider.
#[async_trait(?Send)]
pub trait NetworkTransport {
    /// Send a message payload to a destination address.
    ///
    /// Takes `&self` instead of `&mut self` to allow calling from RefCell borrows.
    ///
    /// # Parameters
    ///
    /// - `destination`: Target node address (e.g., "127.0.0.1:8001")
    /// - `payload`: Serialized message bytes
    ///
    /// # Returns
    ///
    /// - `Ok(response_payload)`: Response bytes from the remote node
    /// - `Err(ActorError)`: Network error occurred
    async fn send(&self, destination: &str, payload: Vec<u8>) -> Result<Vec<u8>, ActorError>;

    /// Poll for incoming messages from the network.
    ///
    /// # Returns
    ///
    /// - `Some(payload)`: Received message bytes
    /// - `None`: No messages available
    fn poll_receive(&mut self) -> Option<Vec<u8>>;
}

/// Wrapper around foundation's ClientTransport implementing NetworkTransport trait.
///
/// This allows MessageBus to use foundation's transport layer without being
/// generic over the provider types. Uses RequestResponseEnvelope for all communication.
pub struct FoundationTransport<N, T, TP>
where
    N: moonpool_foundation::network::NetworkProvider + Clone + 'static,
    T: moonpool_foundation::time::TimeProvider + Clone + 'static,
    TP: moonpool_foundation::task::TaskProvider + Clone + 'static,
{
    transport: std::cell::RefCell<
        moonpool_foundation::network::transport::ClientTransport<
            N,
            T,
            TP,
            moonpool_foundation::network::transport::RequestResponseSerializer,
        >,
    >,
}

impl<N, T, TP> FoundationTransport<N, T, TP>
where
    N: moonpool_foundation::network::NetworkProvider + Clone + 'static,
    T: moonpool_foundation::time::TimeProvider + Clone + 'static,
    TP: moonpool_foundation::task::TaskProvider + Clone + 'static,
{
    /// Create a new FoundationTransport.
    pub fn new(
        transport: moonpool_foundation::network::transport::ClientTransport<
            N,
            T,
            TP,
            moonpool_foundation::network::transport::RequestResponseSerializer,
        >,
    ) -> Self {
        Self {
            transport: std::cell::RefCell::new(transport),
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
    async fn send(&self, destination: &str, payload: Vec<u8>) -> Result<Vec<u8>, ActorError> {
        use moonpool_foundation::network::transport::RequestResponseEnvelopeFactory;

        // Get mutable borrow of ClientTransport from RefCell
        // This is safe because we're in a single-threaded context and the
        // ClientTransport::request() self-drives internally without external calls
        let response = self
            .transport
            .borrow_mut()
            .request::<RequestResponseEnvelopeFactory>(destination, payload)
            .await
            .map_err(|e| ActorError::ProcessingFailed(format!("Network send failed: {}", e)))?;

        Ok(response)
    }

    fn poll_receive(&mut self) -> Option<Vec<u8>> {
        use moonpool_foundation::network::transport::Envelope;

        self.transport
            .borrow_mut()
            .poll_receive()
            .map(|received| received.envelope.payload().to_vec())
    }
}
