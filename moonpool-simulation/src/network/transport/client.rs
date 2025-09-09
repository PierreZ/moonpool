use async_trait::async_trait;

use super::{EnvelopeSerializer, NetTransport, ReceivedEnvelope, TransportDriver, TransportError};
use crate::network::NetworkProvider;
use crate::task::TaskProvider;
use crate::time::TimeProvider;

/// Client-side transport implementation for connecting to servers.
///
/// This transport wraps a TransportDriver and provides client-specific functionality
/// for connecting to remote servers and sending/receiving envelopes.
pub struct ClientTransport<N, T, TP, S>
where
    N: NetworkProvider,
    T: TimeProvider,
    TP: TaskProvider,
    S: EnvelopeSerializer,
{
    /// Underlying transport driver for I/O operations
    driver: TransportDriver<N, T, TP, S>,
    /// Client token for envelope correlation
    client_token: u64,
}

impl<N, T, TP, S> ClientTransport<N, T, TP, S>
where
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
    S: EnvelopeSerializer,
{
    /// Create a new client transport
    pub fn new(serializer: S, network: N, time: T, task_provider: TP) -> Self {
        let driver = TransportDriver::new(serializer, network, time, task_provider);

        Self {
            driver,
            client_token: 2, // Client uses token 2 (server uses 1)
        }
    }

    /// Get the client token used for envelope correlation
    pub fn client_token(&self) -> u64 {
        self.client_token
    }

    /// Get statistics about the underlying driver
    pub fn stats(&self) -> super::TransportDriverStats {
        self.driver.queue_stats()
    }
}

#[async_trait(?Send)]
impl<N, T, TP, S> NetTransport<S> for ClientTransport<N, T, TP, S>
where
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
    S: EnvelopeSerializer,
{
    async fn bind(&mut self, _address: &str) -> Result<(), TransportError> {
        // Clients don't bind - this is a no-op for client transport
        Ok(())
    }

    fn send(&mut self, destination: &str, envelope: S::Envelope) -> Result<(), TransportError> {
        self.driver.send(destination, envelope);
        Ok(())
    }

    fn poll_receive(&mut self) -> Option<ReceivedEnvelope<S::Envelope>> {
        self.driver.poll_receive().map(|envelope| ReceivedEnvelope {
            from: "server".to_string(), // Simplified for Phase 11
            envelope,
        })
    }

    async fn tick(&mut self) {
        // Drive the underlying transport
        self.driver.tick().await;
    }

    async fn close(&mut self) {
        tracing::debug!("ClientTransport: Closing via NetTransport trait");
        self.driver.close().await;
    }
}
