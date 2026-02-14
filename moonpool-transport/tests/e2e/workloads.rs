//! Workload implementations for E2E Peer testing.
//!
//! Provides client and server workloads that can be used with SimulationBuilder
//! to test Peer behavior under various conditions.

#![allow(dead_code)] // Config fields may not all be used in every workload

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{SimContext, SimulationResult, Workload, assert_sometimes};
use moonpool_transport::{
    NetworkProvider, Peer, TcpListenerTrait, TimeProvider, try_deserialize_packet,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::TestMessage;
use super::invariants::MessageInvariants;
use super::operations::{OpResult, OpWeights, execute_operation, generate_operation};

// ============================================================================
// Client Workload
// ============================================================================

/// Configuration for a client workload.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Number of operations to generate.
    pub num_operations: usize,
    /// Operation selection weights.
    pub weights: OpWeights,
    /// Whether to run drain phase after active phase.
    pub drain_phase: bool,
    /// Maximum time to wait for drain phase.
    pub drain_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            num_operations: 500,
            weights: OpWeights::default(),
            drain_phase: true,
            drain_timeout: Duration::from_secs(30),
        }
    }
}

/// Client workload that sends messages to a server via Peer.
pub struct ClientWorkload {
    config: ClientConfig,
}

impl ClientWorkload {
    /// Create a new client workload with the given config.
    pub fn new(config: ClientConfig) -> Self {
        Self { config }
    }
}

#[async_trait(?Send)]
impl Workload for ClientWorkload {
    fn name(&self) -> &str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let my_id = ctx.my_ip().to_string();

        // Find server address - look for "server" peer first, then first peer
        let server_addr = ctx
            .peer("server")
            .or_else(|| ctx.peers().first().map(|(_, ip)| ip.clone()))
            .unwrap_or_default();

        tracing::info!(
            "Client {} starting, connecting to server at {}",
            my_id,
            server_addr
        );

        // Create peer with default config
        let mut peer = Peer::new_with_defaults(ctx.providers().clone(), server_addr.clone());

        // Track messages for invariant validation
        let invariants = Rc::new(RefCell::new(MessageInvariants::new()));

        // Sequence ID counters
        let mut next_reliable_seq = 0u64;
        let mut next_unreliable_seq = 0u64;

        // Active phase: generate and execute operations
        for _op_num in 0..self.config.num_operations {
            let op = generate_operation(
                ctx.random(),
                &mut next_reliable_seq,
                &mut next_unreliable_seq,
                &self.config.weights,
            );

            let result = execute_operation(&mut peer, op.clone(), &my_id, ctx.time()).await;

            // Record results for invariant tracking
            match result {
                OpResult::Sent {
                    sequence_id,
                    reliable,
                } => {
                    invariants.borrow_mut().record_sent(sequence_id, reliable);
                    tracing::trace!(
                        "Client {}: sent {} message seq={}",
                        my_id,
                        if reliable { "reliable" } else { "unreliable" },
                        sequence_id
                    );
                }
                OpResult::Received { message } => {
                    tracing::trace!(
                        "Client {}: received message from {} seq={}",
                        my_id,
                        message.sender_id,
                        message.sequence_id
                    );
                }
                OpResult::Failed { error } => {
                    assert_sometimes!(true, "Client operation should sometimes fail");
                    tracing::trace!("Client {}: operation failed: {}", my_id, error);
                }
                _ => {}
            }

            // Validate invariants continuously
            invariants.borrow().validate_always();
        }

        // Record how many messages we sent
        let summary = invariants.borrow().summary();
        tracing::info!("Client {} active phase complete: {}", my_id, summary);

        // Drain phase: wait for all reliable messages to be delivered
        if self.config.drain_phase {
            tracing::info!("Client {} entering drain phase", my_id);

            let drain_start = ctx.time().now();

            loop {
                let queue_size = peer.queue_size();
                let elapsed = ctx.time().now() - drain_start;

                if queue_size == 0 {
                    tracing::info!("Client {} drain complete: queue empty", my_id);
                    break;
                }

                if elapsed > self.config.drain_timeout {
                    tracing::warn!(
                        "Client {} drain timeout after {:?} with {} messages still queued",
                        my_id,
                        elapsed,
                        queue_size
                    );
                    break;
                }

                // Small delay to allow processing
                let _ = ctx.time().sleep(Duration::from_millis(10)).await;
            }
        }

        // Close peer cleanly
        peer.close().await;

        Ok(())
    }
}

// ============================================================================
// Wire Server Workload
// ============================================================================

/// Configuration for wire-protocol server workload.
#[derive(Debug, Clone)]
pub struct WireServerConfig {
    /// How long to run the server.
    pub run_duration: Duration,
}

impl Default for WireServerConfig {
    fn default() -> Self {
        Self {
            run_duration: Duration::from_secs(60),
        }
    }
}

/// Wire-protocol-aware server that parses Peer wire format.
///
/// This server:
/// 1. Accepts TCP connections
/// 2. Parses wire format: `[length:4][checksum:4][token:16][payload]`
/// 3. Extracts and tracks TestMessage from payload
/// 4. Validates invariants for received messages
pub struct WireServerWorkload {
    config: WireServerConfig,
}

impl WireServerWorkload {
    /// Create a new wire server workload with default config.
    pub fn new() -> Self {
        Self {
            config: WireServerConfig::default(),
        }
    }

    /// Create a new wire server workload with custom config.
    pub fn with_config(config: WireServerConfig) -> Self {
        Self { config }
    }
}

#[async_trait(?Send)]
impl Workload for WireServerWorkload {
    fn name(&self) -> &str {
        "server"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let my_addr = ctx.my_ip().to_string();

        tracing::info!("Wire server {} starting", my_addr);

        // Bind listener
        let listener = ctx.network().bind(&my_addr).await?;
        tracing::info!("Wire server {} listening", my_addr);

        // Track received messages for invariant validation
        let invariants = Rc::new(RefCell::new(MessageInvariants::new()));
        let mut messages_received = 0u64;
        let mut wire_errors = 0u64;

        let start_time = ctx.time().now();

        loop {
            let elapsed = ctx.time().now() - start_time;
            if elapsed > self.config.run_duration {
                tracing::info!("Wire server {} shutting down after {:?}", my_addr, elapsed);
                break;
            }

            // Accept with timeout
            let accept_result = ctx
                .time()
                .timeout(Duration::from_millis(100), listener.accept())
                .await;

            match accept_result {
                Ok(Ok((mut stream, peer_addr))) => {
                    tracing::debug!(
                        "Wire server {} accepted connection from {}",
                        my_addr,
                        peer_addr
                    );

                    // Buffer for accumulating wire data
                    let mut wire_buffer = Vec::new();
                    let mut read_buffer = vec![0u8; 8192];

                    // Read from this connection until closed or timeout
                    loop {
                        let read_result = ctx
                            .time()
                            .timeout(Duration::from_millis(50), stream.read(&mut read_buffer))
                            .await;

                        match read_result {
                            Ok(Ok(0)) => {
                                // Connection closed
                                assert_sometimes!(
                                    true,
                                    "Wire server should sometimes see connections close"
                                );
                                break;
                            }
                            Ok(Ok(n)) => {
                                wire_buffer.extend_from_slice(&read_buffer[..n]);

                                // Try to parse complete packets from the buffer
                                loop {
                                    match try_deserialize_packet(&wire_buffer) {
                                        Ok(Some((_token, payload, consumed))) => {
                                            // Now parse TestMessage from payload
                                            match TestMessage::from_bytes(&payload) {
                                                Ok(msg) => {
                                                    messages_received += 1;
                                                    invariants.borrow_mut().record_received(
                                                        msg.sequence_id,
                                                        msg.sent_reliably,
                                                    );

                                                    tracing::trace!(
                                                        "Wire server {} received message from {} seq={} reliable={}",
                                                        my_addr,
                                                        msg.sender_id,
                                                        msg.sequence_id,
                                                        msg.sent_reliably
                                                    );

                                                    invariants.borrow().validate_receiver_always();
                                                }
                                                Err(e) => {
                                                    tracing::trace!(
                                                        "Wire server {} failed to parse TestMessage: {}",
                                                        my_addr,
                                                        e
                                                    );
                                                    wire_errors += 1;
                                                }
                                            }

                                            // Remove consumed bytes from buffer
                                            wire_buffer.drain(..consumed);
                                        }
                                        Ok(None) => {
                                            // Need more data
                                            break;
                                        }
                                        Err(e) => {
                                            // Wire format error (checksum mismatch, etc.)
                                            assert_sometimes!(
                                                true,
                                                "Wire server should sometimes see wire errors"
                                            );
                                            tracing::trace!(
                                                "Wire server {} wire error: {:?}",
                                                my_addr,
                                                e
                                            );
                                            wire_errors += 1;
                                            // Clear buffer on error - can't recover stream framing
                                            wire_buffer.clear();
                                            break;
                                        }
                                    }
                                }
                            }
                            Ok(Err(e)) => {
                                tracing::trace!("Wire server {} read error: {:?}", my_addr, e);
                                break;
                            }
                            Err(_) => {
                                // Timeout - continue to check duration
                                break;
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::trace!("Wire server {} accept error: {:?}", my_addr, e);
                }
                Err(_) => {
                    // Accept timeout - continue loop
                }
            }
        }

        let summary = invariants.borrow().summary();
        tracing::info!(
            "Wire server {} complete: received {} messages, {} wire errors, {}",
            my_addr,
            messages_received,
            wire_errors,
            summary
        );

        Ok(())
    }
}

// ============================================================================
// Echo Server Workload
// ============================================================================

/// A simpler echo server that reads from accepted connections and writes back.
///
/// NOTE: This server does NOT understand the Peer wire format.
/// For proper E2E testing, use `WireServerWorkload` instead.
pub struct EchoServerWorkload {
    /// How long to run.
    run_duration: Duration,
}

impl EchoServerWorkload {
    /// Create a new echo server workload.
    pub fn new(run_duration: Duration) -> Self {
        Self { run_duration }
    }
}

#[async_trait(?Send)]
impl Workload for EchoServerWorkload {
    fn name(&self) -> &str {
        "echo_server"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let my_addr = ctx.my_ip().to_string();

        tracing::info!("Echo server {} starting", my_addr);

        let listener = ctx.network().bind(&my_addr).await?;
        tracing::info!("Echo server {} listening", my_addr);

        let mut total_bytes = 0u64;
        let start_time = ctx.time().now();

        loop {
            let elapsed = ctx.time().now() - start_time;
            if elapsed > self.run_duration {
                break;
            }

            let accept_result = ctx
                .time()
                .timeout(Duration::from_millis(100), listener.accept())
                .await;

            if let Ok(Ok((mut stream, peer_addr))) = accept_result {
                tracing::debug!("Echo server accepted connection from {}", peer_addr);

                let mut buffer = vec![0u8; 4096];
                loop {
                    let read_result = ctx
                        .time()
                        .timeout(Duration::from_millis(50), stream.read(&mut buffer))
                        .await;

                    match read_result {
                        Ok(Ok(0)) => break, // Connection closed
                        Ok(Ok(n)) => {
                            total_bytes += n as u64;
                            if stream.write_all(&buffer[..n]).await.is_err() {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            }
        }

        tracing::info!(
            "Echo server {} complete: echoed {} bytes",
            my_addr,
            total_bytes
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_config_default() {
        let config = ClientConfig::default();
        assert_eq!(config.num_operations, 500);
        assert!(config.drain_phase);
    }

    #[test]
    fn test_wire_server_config_default() {
        let config = WireServerConfig::default();
        assert_eq!(config.run_duration, Duration::from_secs(60));
    }
}
