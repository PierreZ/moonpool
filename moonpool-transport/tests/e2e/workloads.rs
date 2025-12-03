//! Workload implementations for E2E Peer testing.
//!
//! Provides client and server workloads that can be used with SimulationBuilder
//! to test Peer behavior under various conditions.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use moonpool_sim::{
    SimNetworkProvider, SimRandomProvider, SimTimeProvider, SimulationMetrics, WorkloadTopology,
    sometimes_assert,
};
use moonpool_transport::{
    NetworkProvider, Peer, SimulationResult, TcpListenerTrait, TimeProvider, TokioTaskProvider,
    try_deserialize_packet,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::TestMessage;
use super::invariants::MessageInvariants;
use super::operations::{OpResult, OpWeights, execute_operation, generate_operation};

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

/// Run a client workload that sends messages to a server.
///
/// The client:
/// 1. Creates a Peer connection to the server
/// 2. Executes random operations from the alphabet
/// 3. Tracks sent messages for invariant validation
/// 4. Optionally waits for drain phase (all reliable messages delivered)
pub async fn client_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    client_workload_with_config(
        random,
        network,
        time,
        task_provider,
        topology,
        ClientConfig::default(),
    )
    .await
}

/// Run a client workload with custom configuration.
pub async fn client_workload_with_config(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
    config: ClientConfig,
) -> SimulationResult<SimulationMetrics> {
    let my_id = topology.my_ip.clone();

    // Find server address - look for server prefix first, then fall back to first peer
    let server_addr = topology
        .get_peers_with_prefix("server")
        .first()
        .map(|(_name, ip)| ip.clone())
        .unwrap_or_else(|| {
            // Default to first peer if no server prefix
            topology.peer_ips.first().cloned().unwrap_or_default()
        });

    tracing::info!(
        "Client {} starting, connecting to server at {}",
        my_id,
        server_addr
    );

    // Create peer with default config
    let mut peer = Peer::new_with_defaults(
        network.clone(),
        time.clone(),
        task_provider.clone(),
        server_addr.clone(),
    );

    // Track messages for invariant validation
    let invariants = Rc::new(RefCell::new(MessageInvariants::new()));

    // Sequence ID counters
    let mut next_reliable_seq = 0u64;
    let mut next_unreliable_seq = 0u64;

    // Active phase: generate and execute operations
    for _op_num in 0..config.num_operations {
        let op = generate_operation(
            &random,
            &mut next_reliable_seq,
            &mut next_unreliable_seq,
            &config.weights,
        );

        let result = execute_operation(&mut peer, op.clone(), &my_id, &time).await;

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
                // Client receiving responses (echo from server)
                tracing::trace!(
                    "Client {}: received message from {} seq={}",
                    my_id,
                    message.sender_id,
                    message.sequence_id
                );
            }
            OpResult::Failed { error } => {
                sometimes_assert!(
                    client_operation_failed,
                    true,
                    format!("Client operation should sometimes fail: {}", error)
                );
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
    if config.drain_phase {
        tracing::info!("Client {} entering drain phase", my_id);

        let drain_start = time.now();

        // Wait until queue is empty or timeout
        loop {
            let queue_size = peer.queue_size();
            let elapsed = time.now() - drain_start;

            if queue_size == 0 {
                tracing::info!("Client {} drain complete: queue empty", my_id);
                break;
            }

            if elapsed > config.drain_timeout {
                tracing::warn!(
                    "Client {} drain timeout after {:?} with {} messages still queued",
                    my_id,
                    elapsed,
                    queue_size
                );
                break;
            }

            // Small delay to allow processing
            let _ = time.sleep(Duration::from_millis(10)).await;
        }
    }

    // Close peer cleanly
    peer.close().await;

    Ok(SimulationMetrics {
        events_processed: config.num_operations as u64,
        ..Default::default()
    })
}

/// Configuration for a server workload.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// How long to run the server.
    pub run_duration: Duration,
    /// Whether to echo messages back to sender.
    pub echo_messages: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            run_duration: Duration::from_secs(60),
            echo_messages: false,
        }
    }
}

/// Run a server workload that receives messages from clients.
///
/// The server:
/// 1. Binds a listener and accepts connections
/// 2. Receives messages and tracks them for invariant validation
/// 3. Optionally echoes messages back
#[allow(dead_code)]
pub async fn server_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    server_workload_with_config(
        random,
        network,
        time,
        task_provider,
        topology,
        ServerConfig::default(),
    )
    .await
}

/// Run a server workload with custom configuration.
#[allow(dead_code)]
pub async fn server_workload_with_config(
    _random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    _task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
    config: ServerConfig,
) -> SimulationResult<SimulationMetrics> {
    let my_addr = topology.my_ip.clone();

    tracing::info!("Server {} starting", my_addr);

    // Bind listener
    let listener = network.bind(&my_addr).await?;
    tracing::info!("Server {} listening", my_addr);

    // Track received messages
    let invariants = Rc::new(RefCell::new(MessageInvariants::new()));
    let mut messages_received = 0u64;

    let start_time = time.now();

    // Accept connections and receive messages
    loop {
        let elapsed = time.now() - start_time;
        if elapsed > config.run_duration {
            tracing::info!("Server {} shutting down after {:?}", my_addr, elapsed);
            break;
        }

        // Accept with timeout
        let accept_result = time
            .timeout(Duration::from_millis(100), listener.accept())
            .await;

        match accept_result {
            Ok(Ok(Ok((mut stream, peer_addr)))) => {
                tracing::debug!("Server {} accepted connection from {}", my_addr, peer_addr);

                // Read messages from this connection
                let mut buffer = vec![0u8; 8192];
                let mut read_buffer = Vec::new();

                loop {
                    let read_result = time
                        .timeout(Duration::from_millis(50), stream.read(&mut buffer))
                        .await;

                    match read_result {
                        Ok(Ok(Ok(0))) => {
                            // Connection closed
                            sometimes_assert!(
                                server_connection_closed,
                                true,
                                "Server should sometimes see connections close"
                            );
                            break;
                        }
                        Ok(Ok(Ok(n))) => {
                            read_buffer.extend_from_slice(&buffer[..n]);

                            // Try to parse messages from buffer
                            // Note: This is simplified - real impl would use wire format
                            if read_buffer.len() >= 14 {
                                // Try to deserialize TestMessage
                                match TestMessage::from_bytes(&read_buffer) {
                                    Ok(msg) => {
                                        messages_received += 1;
                                        invariants
                                            .borrow_mut()
                                            .record_received(msg.sequence_id, msg.sent_reliably);

                                        tracing::trace!(
                                            "Server {} received message from {} seq={}",
                                            my_addr,
                                            msg.sender_id,
                                            msg.sequence_id
                                        );

                                        // Validate
                                        invariants.borrow().validate_always();

                                        // Clear buffer (simplified - assumes one message per read)
                                        read_buffer.clear();
                                    }
                                    Err(_) => {
                                        // Invalid data - clear and continue
                                        read_buffer.clear();
                                    }
                                }
                            }
                        }
                        _ => {
                            // Read error or timeout
                            break;
                        }
                    }
                }
            }
            Ok(Ok(Err(e))) => {
                tracing::trace!("Server {} accept error: {:?}", my_addr, e);
            }
            Ok(Err(_)) | Err(_) => {
                // Accept timeout - continue loop
            }
        }
    }

    let summary = invariants.borrow().summary();
    tracing::info!(
        "Server {} complete: received {} messages, {}",
        my_addr,
        messages_received,
        summary
    );

    // Validate at quiescence
    invariants.borrow().validate_at_quiescence();

    Ok(SimulationMetrics {
        events_processed: messages_received,
        ..Default::default()
    })
}

/// A simpler echo server that just reads from accepted connections
/// and writes the same data back.
///
/// NOTE: This server does NOT understand the Peer wire format.
/// For proper E2E testing, use `wire_server_workload` instead.
#[allow(dead_code)]
pub async fn echo_server_workload(
    _random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    _task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let my_addr = topology.my_ip.clone();

    tracing::info!("Echo server {} starting", my_addr);

    // Bind listener
    let listener = network.bind(&my_addr).await?;
    tracing::info!("Echo server {} listening", my_addr);

    let mut total_bytes = 0u64;
    let start_time = time.now();
    let run_duration = Duration::from_secs(30);

    loop {
        let elapsed = time.now() - start_time;
        if elapsed > run_duration {
            break;
        }

        // Accept with timeout
        let accept_result = time
            .timeout(Duration::from_millis(100), listener.accept())
            .await;

        if let Ok(Ok(Ok((mut stream, peer_addr)))) = accept_result {
            tracing::debug!("Echo server accepted connection from {}", peer_addr);

            // Echo data back
            let mut buffer = vec![0u8; 4096];
            loop {
                let read_result = time
                    .timeout(Duration::from_millis(50), stream.read(&mut buffer))
                    .await;

                match read_result {
                    Ok(Ok(Ok(0))) => break, // Connection closed
                    Ok(Ok(Ok(n))) => {
                        total_bytes += n as u64;
                        // Echo back
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

    Ok(SimulationMetrics {
        events_processed: total_bytes,
        ..Default::default()
    })
}

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
///
/// Use this for proper E2E testing of Peer correctness.
pub async fn wire_server_workload(
    _random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    _task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    wire_server_workload_with_config(
        _random,
        network,
        time,
        _task_provider,
        topology,
        WireServerConfig::default(),
    )
    .await
}

/// Wire-protocol-aware server with custom configuration.
pub async fn wire_server_workload_with_config(
    _random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    _task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
    config: WireServerConfig,
) -> SimulationResult<SimulationMetrics> {
    let my_addr = topology.my_ip.clone();

    tracing::info!("Wire server {} starting", my_addr);

    // Bind listener
    let listener = network.bind(&my_addr).await?;
    tracing::info!("Wire server {} listening", my_addr);

    // Track received messages for invariant validation
    let invariants = Rc::new(RefCell::new(MessageInvariants::new()));
    let mut messages_received = 0u64;
    let mut wire_errors = 0u64;

    let start_time = time.now();

    loop {
        let elapsed = time.now() - start_time;
        if elapsed > config.run_duration {
            tracing::info!("Wire server {} shutting down after {:?}", my_addr, elapsed);
            break;
        }

        // Accept with timeout
        let accept_result = time
            .timeout(Duration::from_millis(100), listener.accept())
            .await;

        match accept_result {
            Ok(Ok(Ok((mut stream, peer_addr)))) => {
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
                    let read_result = time
                        .timeout(Duration::from_millis(50), stream.read(&mut read_buffer))
                        .await;

                    match read_result {
                        Ok(Ok(Ok(0))) => {
                            // Connection closed
                            sometimes_assert!(
                                wire_server_connection_closed,
                                true,
                                "Wire server should sometimes see connections close"
                            );
                            break;
                        }
                        Ok(Ok(Ok(n))) => {
                            wire_buffer.extend_from_slice(&read_buffer[..n]);

                            // Try to parse complete packets from the buffer
                            loop {
                                match try_deserialize_packet(&wire_buffer) {
                                    Ok(Some((token, payload, consumed))) => {
                                        // Successfully parsed a wire packet
                                        tracing::trace!(
                                            "Wire server {} parsed packet: token={:?}, payload_len={}",
                                            my_addr,
                                            token,
                                            payload.len()
                                        );

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

                                                // Validate receiver-side invariants (no duplicates)
                                                // Note: We can't validate received⊆sent without cross-workload validation
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
                                        sometimes_assert!(
                                            wire_server_wire_error,
                                            true,
                                            format!(
                                                "Wire server should sometimes see wire errors: {:?}",
                                                e
                                            )
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
                        Ok(Ok(Err(e))) => {
                            tracing::trace!("Wire server {} read error: {:?}", my_addr, e);
                            break;
                        }
                        _ => {
                            // Timeout - continue to check duration
                            break;
                        }
                    }
                }
            }
            Ok(Ok(Err(e))) => {
                tracing::trace!("Wire server {} accept error: {:?}", my_addr, e);
            }
            Ok(Err(_)) | Err(_) => {
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

    // Note: We intentionally do NOT validate at quiescence here.
    // That would require cross-workload validation (comparing client.sent vs server.received).
    // Currently, client tracks what it sent, server tracks what it received - separately.
    // True validation would need StateRegistry or shared state mechanism.

    Ok(SimulationMetrics {
        events_processed: messages_received,
        ..Default::default()
    })
}
