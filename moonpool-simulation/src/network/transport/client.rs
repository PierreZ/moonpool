use async_trait::async_trait;
use std::collections::HashMap;
use std::future::Future;
use tokio::sync::oneshot;

use super::{EnvelopeSerializer, EnvelopeFactory, EnvelopeReplyDetection, NetTransport, ReceivedEnvelope, TransportDriver, TransportError};
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
    /// Next correlation ID for requests
    next_correlation_id: u64,
    /// Pending requests awaiting responses
    pending_requests: HashMap<u64, oneshot::Sender<Vec<u8>>>,
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
            next_correlation_id: 1,
            pending_requests: HashMap::new(),
        }
    }

    /// Generate next correlation ID
    fn next_correlation_id(&mut self) -> u64 {
        let id = self.next_correlation_id;
        self.next_correlation_id += 1;
        id
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
    S::Envelope: EnvelopeReplyDetection + EnvelopeFactory<S>,
{
    async fn bind(&mut self, _address: &str) -> Result<(), TransportError> {
        // Clients don't bind - this is a no-op for client transport
        Ok(())
    }

    fn get_reply<E>(&mut self, destination: &str, payload: Vec<u8>) -> Result<impl Future<Output = Result<Vec<u8>, TransportError>>, TransportError>
    where
        E: EnvelopeFactory<S> + EnvelopeReplyDetection + 'static,
    {
        let correlation_id = self.next_correlation_id();
        let (tx, rx) = oneshot::channel();
        
        // Store the response channel
        self.pending_requests.insert(correlation_id, tx);
        
        // Create envelope using the factory
        let envelope = E::create_request(correlation_id, payload);
        
        // Send the request through the driver
        self.driver.send(destination, envelope);
        
        // Return future that resolves when response arrives
        Ok(async move {
            match rx.await {
                Ok(response_payload) => Ok(response_payload),
                Err(_) => Err(TransportError::SendFailed("Request cancelled or timed out".to_string()))
            }
        })
    }
    
    fn send_reply(&mut self, request: &S::Envelope, payload: Vec<u8>) -> Result<(), TransportError>
    where
        S::Envelope: EnvelopeFactory<S>,
    {
        // Clients typically don't send replies, but we implement it for completeness
        let reply_envelope = S::Envelope::create_reply(request, payload);
        self.driver.send("server", reply_envelope); // Simplified destination
        Ok(())
    }

    fn poll_receive(&mut self) -> Option<ReceivedEnvelope<S::Envelope>> {
        while let Some(envelope) = self.driver.poll_receive() {
            // Check if this is a response to a pending request
            if let Some(correlation_id) = envelope.correlation_id() {
                if let Some(tx) = self.pending_requests.remove(&correlation_id) {
                    // This is a response - send it to the waiting future
                    let payload = S::Envelope::extract_payload(&envelope).to_vec();
                    let _ = tx.send(payload);
                    continue; // Don't return this envelope
                }
            }
            
            // This is an unsolicited message - return it
            return Some(ReceivedEnvelope {
                from: "server".to_string(), // Simplified for Phase 11
                envelope,
            });
        }
        None
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
