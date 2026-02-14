// TODO CLAUDE AI: port to new Workload trait API
/*
//! Workload implementations for simulation testing.
//!
//! Provides client and server workloads that exercise NetTransport,
//! EndpointMap, and NetNotifiedQueue under chaos conditions.
//!
//! # Phase 12C APIs
//!
//! These workloads use the Phase 12C developer experience improvements:
//! - [`NetTransportBuilder`] - Automatic `Rc` wrapping and `set_weak_self()`
//! - [`register_handler()`] - Type-safe endpoint registration
//! - [`recv_with_transport()`] - Clean async receive without closures

#![allow(dead_code)] // Config fields may not all be used in every workload

use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::time::Duration;

use moonpool_sim::{SimRandomProvider, SimulationMetrics, WorkloadTopology};
use moonpool_transport::{
    Endpoint, JsonCodec, MessageReceiver, NetNotifiedQueue, NetTransportBuilder, NetworkAddress,
    NetworkProvider, Providers, SimulationResult, StorageProvider, TaskProvider, TimeProvider,
    TokioStorageProvider, UID,
};

/// Providers bundle for simulation workloads.
///
/// This struct bundles the individual providers passed to workloads by the simulation
/// framework into a single `Providers` implementation.
#[derive(Clone)]
pub struct WorkloadProviders<N, T, TP, S = TokioStorageProvider>
where
    S: StorageProvider + Clone + 'static,
{
    network: N,
    time: T,
    task: TP,
    random: SimRandomProvider,
    // TODO: Replace default with SimStorageProvider when implemented
    storage: S,
}

impl<N, T, TP> WorkloadProviders<N, T, TP, TokioStorageProvider>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    /// Create a new workload providers bundle.
    pub fn new(network: N, time: T, task: TP, random: SimRandomProvider) -> Self {
        Self {
            network,
            time,
            task,
            random,
            storage: TokioStorageProvider::new(),
        }
    }
}

impl<N, T, TP, S> Providers for WorkloadProviders<N, T, TP, S>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    S: StorageProvider + Clone + 'static,
{
    type Network = N;
    type Time = T;
    type Task = TP;
    type Random = SimRandomProvider;
    // TODO: Replace with SimStorageProvider when implemented
    type Storage = S;

    fn network(&self) -> &Self::Network {
        &self.network
    }

    fn time(&self) -> &Self::Time {
        &self.time
    }

    fn task(&self) -> &Self::Task {
        &self.task
    }

    fn random(&self) -> &Self::Random {
        &self.random
    }

    fn storage(&self) -> &Self::Storage {
        &self.storage
    }
}

use super::invariants::{MessageInvariants, RpcInvariants};
use super::operations::{
    ClientOp, ClientOpWeights, RpcClientOp, RpcClientOpWeights, RpcServerOp, RpcServerOpWeights,
    generate_client_op, generate_rpc_client_op, generate_rpc_server_op,
};
use super::{RpcTestRequest, RpcTestResponse, TestMessage};

/// Configuration for local delivery workload.
#[derive(Debug, Clone)]
pub struct LocalDeliveryConfig {
    /// Number of operations to perform
    pub num_operations: u64,
    /// Operation weights
    pub weights: ClientOpWeights,
    /// Timeout for receive operations
    pub receive_timeout: Duration,
}

impl Default for LocalDeliveryConfig {
    fn default() -> Self {
        Self {
            num_operations: 100,
            weights: ClientOpWeights::default(),
            receive_timeout: Duration::from_secs(5),
        }
    }
}

/// Local delivery workload - tests NetTransport dispatch to local endpoints.
///
/// This workload:
/// 1. Creates a NetTransport with a local address (using NetTransportBuilder)
/// 2. Registers a NetNotifiedQueue as an endpoint
/// 3. Sends messages to the local endpoint
/// 4. Verifies messages are dispatched correctly
///
/// Note: Uses raw NetNotifiedQueue (not RequestStream) since this tests direct
/// message dispatch, not RPC patterns.
pub async fn local_delivery_workload<N, T, TP>(
    random: SimRandomProvider,
    network: N,
    time: T,
    task: TP,
    topology: WorkloadTopology,
    config: LocalDeliveryConfig,
) -> SimulationResult<SimulationMetrics>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    let my_id = topology.my_ip.clone();

    // Create local address from topology
    let local_addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500);

    // Bundle providers for NetTransportBuilder
    let providers = WorkloadProviders::new(network, time.clone(), task, random.clone());

    // Create NetTransport using Phase 12C builder (no listening needed for local delivery)
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr.clone())
        .build()
        .expect("build should succeed");

    // Create message queue and register with transport
    // Note: Using raw NetNotifiedQueue since this tests direct dispatch, not RPC
    let token = UID::new(0x1234, 0x5678);
    let queue: Rc<NetNotifiedQueue<TestMessage, JsonCodec>> = Rc::new(NetNotifiedQueue::new(
        Endpoint::new(local_addr.clone(), token),
        JsonCodec,
    ));
    let endpoint = transport.register(token, queue.clone() as Rc<dyn MessageReceiver>);

    // Track invariants
    let invariants = Rc::new(RefCell::new(MessageInvariants::new()));

    // Sequence counters
    let mut next_reliable_seq = 0u64;
    let mut next_unreliable_seq = 10000u64; // Offset to distinguish

    // Perform operations
    for _op_num in 0..config.num_operations {
        let op = generate_client_op(
            &random,
            &mut next_reliable_seq,
            &mut next_unreliable_seq,
            &config.weights,
        );

        match op {
            ClientOp::SendReliable {
                seq_id,
                payload_size,
            } => {
                let msg = TestMessage::with_payload(seq_id, &my_id, true, payload_size);
                let payload = msg.to_bytes();

                match transport.send_reliable(&endpoint, &payload) {
                    Ok(()) => {
                        invariants.borrow_mut().record_sent(seq_id, true);
                    }
                    Err(e) => {
                        // Send to local endpoint shouldn't fail if registered
                        tracing::warn!(error = %e, "Reliable send failed");
                    }
                }
            }
            ClientOp::SendUnreliable {
                seq_id,
                payload_size,
            } => {
                let msg = TestMessage::with_payload(seq_id, &my_id, false, payload_size);
                let payload = msg.to_bytes();

                match transport.send_unreliable(&endpoint, &payload) {
                    Ok(()) => {
                        invariants.borrow_mut().record_sent(seq_id, false);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Unreliable send failed");
                    }
                }
            }
            ClientOp::SmallDelay => {
                let _ = time.sleep(Duration::from_millis(1)).await;
            }
        }

        // Drain the queue and validate received messages
        while let Some(msg) = queue.try_recv() {
            let is_dup = invariants
                .borrow_mut()
                .record_received(msg.seq_id, msg.reliable);
            if !is_dup {
                invariants.borrow().validate_always();
            }
        }
    }

    // Final drain and validation
    while let Some(msg) = queue.try_recv() {
        invariants
            .borrow_mut()
            .record_received(msg.seq_id, msg.reliable);
    }

    let inv = invariants.borrow();
    inv.validate_always();

    // Build metrics
    Ok(SimulationMetrics {
        events_processed: config.num_operations,
        ..Default::default()
    })
}

/// Endpoint lifecycle workload - tests register/unregister under send load.
///
/// This workload:
/// 1. Creates multiple endpoints (using NetTransportBuilder)
/// 2. Sends messages while registering/unregistering endpoints
/// 3. Verifies endpoint_not_found is triggered for unregistered endpoints
///
/// Note: Uses raw NetNotifiedQueue (not RequestStream) since this tests
/// endpoint lifecycle management, not RPC patterns.
pub async fn endpoint_lifecycle_workload<N, T, TP>(
    random: SimRandomProvider,
    network: N,
    time: T,
    task: TP,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    let my_id = topology.my_ip.clone();
    let local_addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500);

    // Bundle providers for NetTransportBuilder
    let providers = WorkloadProviders::new(network, time.clone(), task, random);

    // Create NetTransport using Phase 12C builder
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr.clone())
        .build()
        .expect("build should succeed");

    // Track sent/received
    let mut sent_count = 0u64;
    let mut _received_count = 0u64;
    let mut undelivered_count = 0u64;

    // Create initial endpoint
    let token1 = UID::new(0x1111, 0x1111);
    let queue1: Rc<NetNotifiedQueue<TestMessage, JsonCodec>> = Rc::new(NetNotifiedQueue::new(
        Endpoint::new(local_addr.clone(), token1),
        JsonCodec,
    ));
    let endpoint1 = transport.register(token1, queue1.clone() as Rc<dyn MessageReceiver>);

    // Second endpoint for lifecycle testing
    let token2 = UID::new(0x2222, 0x2222);
    let queue2: Rc<NetNotifiedQueue<TestMessage, JsonCodec>> = Rc::new(NetNotifiedQueue::new(
        Endpoint::new(local_addr.clone(), token2),
        JsonCodec,
    ));

    for i in 0..50 {
        // Send to endpoint1 (always registered)
        let msg = TestMessage::new(i, &my_id, true);
        let payload = msg.to_bytes();
        if transport.send_reliable(&endpoint1, &payload).is_ok() {
            sent_count += 1;
        }

        // Toggle endpoint2 registration
        if i % 10 == 0 {
            let endpoint2 = transport.register(token2, queue2.clone() as Rc<dyn MessageReceiver>);

            // Send to endpoint2 while registered
            let msg2 = TestMessage::new(i + 1000, &my_id, true);
            let payload2 = msg2.to_bytes();
            if transport.send_reliable(&endpoint2, &payload2).is_ok() {
                sent_count += 1;
            }

            // Unregister endpoint2
            transport.unregister(&token2);
        }

        // Try sending to unregistered endpoint2
        if i % 10 == 5 {
            let endpoint2 = Endpoint::new(local_addr.clone(), token2);
            let msg2 = TestMessage::new(i + 2000, &my_id, true);
            let payload2 = msg2.to_bytes();
            if transport.send_reliable(&endpoint2, &payload2).is_err() {
                undelivered_count += 1;
            }
        }

        // Drain queues
        while let Some(_msg) = queue1.try_recv() {
            _received_count += 1;
        }
        while let Some(_msg) = queue2.try_recv() {
            _received_count += 1;
        }

        let _ = time.sleep(Duration::from_millis(1)).await;
    }

    // Final drain
    while let Some(_msg) = queue1.try_recv() {
        _received_count += 1;
    }
    while let Some(_msg) = queue2.try_recv() {
        _received_count += 1;
    }

    Ok(SimulationMetrics {
        events_processed: sent_count + undelivered_count,
        ..Default::default()
    })
}

// ============================================================================
// RPC Workload (Phase 12B)
// ============================================================================

use moonpool_sim::sometimes_assert;
use moonpool_transport::{ReplyPromise, send_request};

/// Configuration for RPC workload.
#[derive(Debug, Clone)]
pub struct RpcWorkloadConfig {
    /// Number of operations to perform per workload
    pub num_operations: u64,
    /// Weights for client operations
    pub client_weights: RpcClientOpWeights,
    /// Weights for server operations
    pub server_weights: RpcServerOpWeights,
    /// Timeout for awaiting RPC responses
    pub request_timeout: Duration,
}

impl Default for RpcWorkloadConfig {
    fn default() -> Self {
        Self {
            num_operations: 100,
            client_weights: RpcClientOpWeights::default(),
            server_weights: RpcServerOpWeights::default(),
            request_timeout: Duration::from_secs(5),
        }
    }
}

impl RpcWorkloadConfig {
    /// Configuration focused on happy path testing (no dropped promises).
    pub fn happy_path() -> Self {
        Self {
            num_operations: 50,
            client_weights: RpcClientOpWeights::default(),
            server_weights: RpcServerOpWeights::happy_path(),
            request_timeout: Duration::from_secs(5),
        }
    }

    /// Configuration focused on broken promise testing.
    pub fn broken_promise_focused() -> Self {
        Self {
            num_operations: 100,
            client_weights: RpcClientOpWeights::default(),
            server_weights: RpcServerOpWeights::broken_promise_focused(),
            request_timeout: Duration::from_secs(5),
        }
    }
}

/// RPC workload - tests request-response patterns with ReplyPromise/ReplyFuture.
///
/// This workload uses Phase 12C APIs:
/// 1. Creates a NetTransport with [`NetTransportBuilder::build()`]
/// 2. Registers RequestStream via [`register_handler()`]
/// 3. Client sends RPC requests
/// 4. Server processes requests via [`try_recv_with_transport()`]
/// 5. Validates RPC invariants throughout
pub async fn rpc_workload<N, T, TP>(
    random: SimRandomProvider,
    network: N,
    time: T,
    task: TP,
    topology: WorkloadTopology,
    config: RpcWorkloadConfig,
) -> SimulationResult<SimulationMetrics>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    let my_id = topology.my_ip.clone();

    // Create local address from topology
    let local_addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500);

    // Bundle providers for NetTransportBuilder
    let providers = WorkloadProviders::new(network, time.clone(), task, random.clone());

    // Create NetTransport using Phase 12C builder (local only, no network listening)
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr.clone())
        .build()
        .expect("build should succeed");

    // Register handler using Phase 12C API - creates RequestStream and registers in one call
    let server_token = UID::new(0xAABB, 0xCCDD);
    let request_stream = transport.register_handler::<RpcTestRequest, _>(server_token, JsonCodec);
    let server_endpoint = request_stream.endpoint().clone();

    // Track invariants
    let invariants = Rc::new(RefCell::new(RpcInvariants::new()));

    // State tracking
    let mut next_request_id = 0u64;
    let mut pending_requests: Vec<u64> = Vec::new();
    // Track requests received by server but not yet responded to
    // Phase 12C: Store ReplyPromise instead of raw Endpoint
    let mut pending_server_requests: Vec<(u64, ReplyPromise<RpcTestResponse, JsonCodec>)> =
        Vec::new();

    // Perform operations
    for _op_num in 0..config.num_operations {
        // Client operation
        let client_op = generate_rpc_client_op(
            &random,
            &mut next_request_id,
            &pending_requests,
            &config.client_weights,
        );

        match client_op {
            RpcClientOp::SendRequest { request_id } => {
                let request = RpcTestRequest::with_payload(request_id, &my_id, 32);
                match send_request::<_, RpcTestResponse, _, _>(
                    &transport,
                    &server_endpoint,
                    request,
                    JsonCodec,
                ) {
                    Ok(_future) => {
                        invariants.borrow_mut().record_request_sent(request_id);
                        pending_requests.push(request_id);
                        sometimes_assert!(
                            rpc_request_sent,
                            true,
                            "Should be able to send RPC requests"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "RPC request send failed");
                        invariants.borrow_mut().record_connection_failure();
                    }
                }
            }
            RpcClientOp::AwaitResponse { request_id } => {
                // Note: In a real workload, we'd await the actual future
                // For now, we just track that we attempted to await
                pending_requests.retain(|&id| id != request_id);
            }
            RpcClientOp::AwaitWithTimeout {
                request_id,
                timeout_ms,
            } => {
                // Simulate timeout-based await: wait for timeout, then check if response arrived
                // In this local workload, responses are immediate if server processes them
                let timeout_duration = Duration::from_millis(timeout_ms);

                // Check if server has the request pending (hasn't responded yet)
                let server_has_pending = pending_server_requests
                    .iter()
                    .any(|(id, _)| *id == request_id);

                if server_has_pending {
                    // Server hasn't responded - this will timeout
                    let _ = time.sleep(timeout_duration).await;
                    sometimes_assert!(
                        rpc_timeout_path,
                        true,
                        "Client should exercise timeout paths when server is slow"
                    );
                    tracing::debug!(request_id, timeout_ms, "RPC request timed out");
                }

                // Remove from pending regardless (cleanup)
                pending_requests.retain(|&id| id != request_id);
            }
            RpcClientOp::SmallDelay => {
                let _ = time.sleep(Duration::from_millis(1)).await;
            }
        }

        // Server operation - process incoming requests
        // First, check if we have any pending promises to work with
        let pending_ids: Vec<u64> = pending_server_requests.iter().map(|(id, _)| *id).collect();
        let server_op = generate_rpc_server_op(&random, &pending_ids, &config.server_weights);

        match server_op {
            RpcServerOp::TryReceiveRequest => {
                // Phase 12C: Use try_recv_with_transport() instead of queue().try_recv()
                if let Some((request, reply)) =
                    request_stream.try_recv_with_transport::<_, RpcTestResponse>(&transport)
                {
                    sometimes_assert!(
                        rpc_server_received_request,
                        true,
                        "Server should receive RPC requests"
                    );

                    // Track the pending request with its ReplyPromise
                    pending_server_requests.push((request.request_id, reply));
                }
            }
            RpcServerOp::SendResponse { request_id } => {
                // Find and respond to the pending request
                if let Some(pos) = pending_server_requests
                    .iter()
                    .position(|(id, _)| *id == request_id)
                {
                    let (req_id, reply) = pending_server_requests.remove(pos);

                    // Phase 12C: Use ReplyPromise::send() instead of manual serialization
                    reply.send(RpcTestResponse::success(req_id));

                    invariants.borrow_mut().record_response_received(req_id);
                    sometimes_assert!(rpc_response_sent, true, "Server should send RPC responses");
                }
            }
            RpcServerOp::DropPromise { request_id } => {
                // Drop the promise without responding (triggers broken promise automatically)
                if let Some(pos) = pending_server_requests
                    .iter()
                    .position(|(id, _)| *id == request_id)
                {
                    let (req_id, reply) = pending_server_requests.remove(pos);
                    // Phase 12C: Dropping ReplyPromise triggers BrokenPromise automatically
                    drop(reply);
                    invariants.borrow_mut().record_broken_promise(req_id);
                    sometimes_assert!(
                        rpc_promise_dropped,
                        true,
                        "Server should sometimes drop promises"
                    );
                }
            }
            RpcServerOp::SmallDelay => {
                let _ = time.sleep(Duration::from_millis(1)).await;
            }
        }

        // Validate invariants after each operation pair
        invariants.borrow().validate_always();
    }

    // Drain remaining requests - respond to all pending
    // Phase 12C: Use try_recv_with_transport()
    while let Some((request, reply)) =
        request_stream.try_recv_with_transport::<_, RpcTestResponse>(&transport)
    {
        pending_server_requests.push((request.request_id, reply));
    }

    // Respond to all remaining pending requests
    for (req_id, reply) in pending_server_requests.drain(..) {
        // Phase 12C: Use ReplyPromise::send()
        reply.send(RpcTestResponse::success(req_id));
        invariants.borrow_mut().record_response_received(req_id);
    }

    // Final validation
    let inv = invariants.borrow();
    inv.validate_always();
    inv.validate_coverage();

    tracing::info!(summary = %inv.summary(), "RPC workload completed");

    Ok(SimulationMetrics {
        events_processed: config.num_operations,
        ..Default::default()
    })
}

// ============================================================================
// Multi-Node RPC Workloads (Phase 12 Step 7d)
// ============================================================================

/// Configuration for multi-node RPC server workload.
#[derive(Debug, Clone)]
pub struct MultiNodeServerConfig {
    /// Number of operations to perform
    pub num_operations: u64,
    /// Weights for server operations
    pub server_weights: RpcServerOpWeights,
}

impl Default for MultiNodeServerConfig {
    fn default() -> Self {
        Self {
            num_operations: 100,
            server_weights: RpcServerOpWeights::default(),
        }
    }
}

impl MultiNodeServerConfig {
    /// Configuration for happy path testing.
    pub fn happy_path() -> Self {
        Self {
            num_operations: 50,
            server_weights: RpcServerOpWeights::happy_path(),
        }
    }
}

/// Configuration for multi-node RPC client workload.
#[derive(Debug, Clone)]
pub struct MultiNodeClientConfig {
    /// Number of operations to perform
    pub num_operations: u64,
    /// Weights for client operations
    pub client_weights: RpcClientOpWeights,
    /// Timeout for RPC responses
    pub request_timeout: Duration,
}

impl Default for MultiNodeClientConfig {
    fn default() -> Self {
        Self {
            num_operations: 100,
            client_weights: RpcClientOpWeights::default(),
            request_timeout: Duration::from_secs(5),
        }
    }
}

impl MultiNodeClientConfig {
    /// Configuration for happy path testing.
    pub fn happy_path() -> Self {
        Self {
            num_operations: 50,
            client_weights: RpcClientOpWeights::default(),
            request_timeout: Duration::from_secs(5),
        }
    }
}

/// Multi-node RPC server workload.
///
/// This workload uses Phase 12C APIs:
/// 1. Creates a listening NetTransport with [`NetTransportBuilder::build_listening()`]
/// 2. Registers RequestStream via [`register_handler()`]
/// 3. Processes incoming RPC requests via [`try_recv_with_transport()`]
/// 4. Sends responses via [`ReplyPromise::send()`]
/// 5. Publishes invariant state to StateRegistry for cross-node validation
///
/// # FDB Alignment
/// - Uses FDB's `listen()` pattern (FlowTransport.actor.cpp:1646)
/// - Server receives via connection_incoming → connection_reader → dispatch
pub async fn multi_node_rpc_server_workload<N, T, TP>(
    random: SimRandomProvider,
    network: N,
    time: T,
    task: TP,
    topology: WorkloadTopology,
    config: MultiNodeServerConfig,
) -> SimulationResult<SimulationMetrics>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    let my_id = topology.my_ip.clone();
    let state_registry = topology.state_registry.clone();
    tracing::info!(server_id = %my_id, "Starting multi-node RPC server");

    // Parse server address from topology (append port if not present)
    let addr_with_port = if topology.my_ip.contains(':') {
        topology.my_ip.clone()
    } else {
        format!("{}:4500", topology.my_ip)
    };
    let local_addr =
        NetworkAddress::parse(&addr_with_port).expect("valid server address in topology");

    // Bundle providers for NetTransportBuilder
    let providers = WorkloadProviders::new(network, time.clone(), task, random.clone());

    // Phase 12C: Use NetTransportBuilder::build_listening()
    // Automatically handles Rc wrapping, set_weak_self(), and listen()
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr.clone())
        .build_listening()
        .await
        .expect("server listen failed");

    tracing::info!(addr = %local_addr, "Server listening");

    sometimes_assert!(
        multi_node_server_listening,
        true,
        "Server should start listening"
    );

    // Phase 12C: Use register_handler() instead of manual registration
    let server_token = UID::new(0xAABB, 0xCCDD);
    let request_stream = transport.register_handler::<RpcTestRequest, _>(server_token, JsonCodec);

    // Track invariants
    let invariants = Rc::new(RefCell::new(RpcInvariants::new()));

    // Track pending requests (received but not yet responded)
    // Phase 12C: Store ReplyPromise instead of raw Endpoint
    let mut pending_server_requests: Vec<(u64, ReplyPromise<RpcTestResponse, JsonCodec>)> =
        Vec::new();

    // Process operations
    for _op_num in 0..config.num_operations {
        // Server operation
        let pending_ids: Vec<u64> = pending_server_requests.iter().map(|(id, _)| *id).collect();
        let server_op = generate_rpc_server_op(&random, &pending_ids, &config.server_weights);

        match server_op {
            RpcServerOp::TryReceiveRequest => {
                // Phase 12C: Use try_recv_with_transport() instead of queue().try_recv()
                if let Some((request, reply)) =
                    request_stream.try_recv_with_transport::<_, RpcTestResponse>(&transport)
                {
                    sometimes_assert!(
                        multi_node_server_received_request,
                        true,
                        "Server should receive RPC requests from network"
                    );

                    tracing::debug!(request_id = request.request_id, "Server received request");

                    pending_server_requests.push((request.request_id, reply));
                }
            }
            RpcServerOp::SendResponse { request_id } => {
                if let Some(pos) = pending_server_requests
                    .iter()
                    .position(|(id, _)| *id == request_id)
                {
                    let (req_id, reply) = pending_server_requests.remove(pos);

                    // Phase 12C: Use ReplyPromise::send() instead of manual serialization
                    reply.send(RpcTestResponse::success(req_id));

                    invariants.borrow_mut().record_response_received(req_id);
                    sometimes_assert!(
                        multi_node_server_sent_response,
                        true,
                        "Server should send RPC responses"
                    );
                    tracing::debug!(request_id = req_id, "Server sent response");
                }
            }
            RpcServerOp::DropPromise { request_id } => {
                if let Some(pos) = pending_server_requests
                    .iter()
                    .position(|(id, _)| *id == request_id)
                {
                    let (req_id, reply) = pending_server_requests.remove(pos);
                    // Phase 12C: Dropping ReplyPromise triggers BrokenPromise automatically
                    drop(reply);
                    invariants.borrow_mut().record_broken_promise(req_id);
                    sometimes_assert!(
                        multi_node_server_dropped_promise,
                        true,
                        "Server should sometimes drop promises"
                    );
                }
            }
            RpcServerOp::SmallDelay => {
                let _ = time.sleep(Duration::from_millis(1)).await;
            }
        }

        // Note: Server does NOT call validate_always() - that method checks if responses
        // correspond to requests WE sent, but the server doesn't send requests.
        // Server-side validation happens via cross-node invariants in MultiNodeRpcInvariants.

        // Publish state for cross-node validation
        state_registry.register_state(
            format!("server:{}", my_id),
            serde_json::to_value(invariants.borrow().summary()).expect("serialize summary"),
        );
    }

    // Drain remaining requests - respond to all pending
    // Phase 12C: Use try_recv_with_transport()
    while let Some((request, reply)) =
        request_stream.try_recv_with_transport::<_, RpcTestResponse>(&transport)
    {
        pending_server_requests.push((request.request_id, reply));
    }

    // Respond to all remaining pending requests
    for (req_id, reply) in pending_server_requests.drain(..) {
        // Phase 12C: Use ReplyPromise::send()
        reply.send(RpcTestResponse::success(req_id));
        invariants.borrow_mut().record_response_received(req_id);
    }

    // Final state update
    state_registry.register_state(
        format!("server:{}", my_id),
        serde_json::to_value(invariants.borrow().summary()).expect("serialize summary"),
    );

    tracing::info!(summary = %invariants.borrow().summary(), "Multi-node server completed");

    Ok(SimulationMetrics {
        events_processed: config.num_operations,
        ..Default::default()
    })
}

/// Multi-node RPC client workload.
///
/// This workload uses Phase 12C APIs:
/// 1. Creates a NetTransport with [`NetTransportBuilder::build()`]
/// 2. Finds the server via topology
/// 3. Sends RPC requests to the server over the network
/// 4. Awaits responses via ReplyFuture
/// 5. Publishes invariant state to StateRegistry for cross-node validation
///
/// # FDB Alignment
/// - Uses FDB's connection establishment via Peer
/// - Client sends via get_or_open_peer → connection_reader reads responses
pub async fn multi_node_rpc_client_workload<N, T, TP>(
    random: SimRandomProvider,
    network: N,
    time: T,
    task: TP,
    topology: WorkloadTopology,
    config: MultiNodeClientConfig,
) -> SimulationResult<SimulationMetrics>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    let my_id = topology.my_ip.clone();
    let state_registry = topology.state_registry.clone();
    tracing::info!(client_id = %my_id, "Starting multi-node RPC client");

    // Parse client address from topology (append port if not present)
    let addr_with_port = if topology.my_ip.contains(':') {
        topology.my_ip.clone()
    } else {
        format!("{}:4501", topology.my_ip) // Client uses different port
    };
    let local_addr =
        NetworkAddress::parse(&addr_with_port).expect("valid client address in topology");

    // Find server from topology (server registered first gets 10.0.0.1)
    // We need to find a peer that looks like the server
    let server_addr_str = topology
        .peer_ips
        .iter()
        .find(|p| p.contains("10.0.0.1"))
        .or_else(|| topology.peer_ips.first())
        .expect("server address in topology");

    // Append port to server address if needed
    let server_addr_with_port = if server_addr_str.contains(':') {
        server_addr_str.clone()
    } else {
        format!("{}:4500", server_addr_str) // Server listens on 4500
    };
    let server_addr = NetworkAddress::parse(&server_addr_with_port).expect("valid server address");

    tracing::info!(server = %server_addr, "Client connecting to server");

    // Bundle providers for NetTransportBuilder
    let providers = WorkloadProviders::new(network, time.clone(), task, random.clone());

    // Phase 12C: Use NetTransportBuilder::build()
    // Client doesn't need listen() - responses come on outbound connections
    // The builder automatically handles Rc wrapping and set_weak_self()
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr.clone())
        .build()
        .expect("build should succeed");

    // Server endpoint (well-known token)
    let server_token = UID::new(0xAABB, 0xCCDD);
    let server_endpoint = Endpoint::new(server_addr.clone(), server_token);

    // Track invariants
    let invariants = Rc::new(RefCell::new(RpcInvariants::new()));

    // State tracking
    let mut next_request_id = 0u64;
    let mut pending_requests: Vec<u64> = Vec::new();

    // Perform operations
    for _op_num in 0..config.num_operations {
        let client_op = generate_rpc_client_op(
            &random,
            &mut next_request_id,
            &pending_requests,
            &config.client_weights,
        );

        match client_op {
            RpcClientOp::SendRequest { request_id } => {
                let request = RpcTestRequest::with_payload(request_id, &my_id, 32);
                match send_request::<_, RpcTestResponse, _, _>(
                    &transport,
                    &server_endpoint,
                    request,
                    JsonCodec,
                ) {
                    Ok(_future) => {
                        invariants.borrow_mut().record_request_sent(request_id);
                        pending_requests.push(request_id);
                        sometimes_assert!(
                            multi_node_client_sent_request,
                            true,
                            "Client should send RPC requests over network"
                        );
                        tracing::debug!(request_id, "Client sent request");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "RPC request send failed");
                        invariants.borrow_mut().record_connection_failure();
                    }
                }
            }
            RpcClientOp::AwaitResponse { request_id } => {
                // In a real workload with actual futures, we'd await here
                // For now, we simulate completion tracking
                pending_requests.retain(|&id| id != request_id);
            }
            RpcClientOp::AwaitWithTimeout {
                request_id,
                timeout_ms,
            } => {
                // Simulate timeout-based await in multi-node scenario
                // In real code, this would use time.timeout() around the future
                let timeout_duration = Duration::from_millis(timeout_ms);
                let _ = time.sleep(timeout_duration).await;

                // In multi-node, we can't easily check server state, so just simulate
                // that some timeouts occur (chaos will cause real timeouts)
                sometimes_assert!(
                    multi_node_client_timeout_path,
                    pending_requests.contains(&request_id),
                    "Client should exercise timeout paths in multi-node scenarios"
                );
                tracing::debug!(request_id, timeout_ms, "Client await with timeout");

                pending_requests.retain(|&id| id != request_id);
            }
            RpcClientOp::SmallDelay => {
                let _ = time.sleep(Duration::from_millis(1)).await;
            }
        }

        // Validate invariants after each operation
        invariants.borrow().validate_always();

        // Publish state for cross-node validation
        state_registry.register_state(
            format!("client:{}", my_id),
            serde_json::to_value(invariants.borrow().summary()).expect("serialize summary"),
        );
    }

    // Final state update
    state_registry.register_state(
        format!("client:{}", my_id),
        serde_json::to_value(invariants.borrow().summary()).expect("serialize summary"),
    );

    tracing::info!(summary = %invariants.borrow().summary(), "Multi-node client completed");

    Ok(SimulationMetrics {
        events_processed: config.num_operations,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_delivery_config_default() {
        let config = LocalDeliveryConfig::default();
        assert_eq!(config.num_operations, 100);
    }

    #[test]
    fn test_rpc_workload_config_default() {
        let config = RpcWorkloadConfig::default();
        assert_eq!(config.num_operations, 100);
    }

    #[test]
    fn test_rpc_workload_config_presets() {
        let happy = RpcWorkloadConfig::happy_path();
        assert_eq!(happy.server_weights.drop_promise, 0);

        let broken = RpcWorkloadConfig::broken_promise_focused();
        assert_eq!(broken.server_weights.drop_promise, 30);
    }

    #[test]
    fn test_multi_node_server_config_default() {
        let config = MultiNodeServerConfig::default();
        assert_eq!(config.num_operations, 100);
    }

    #[test]
    fn test_multi_node_server_config_happy_path() {
        let config = MultiNodeServerConfig::happy_path();
        assert_eq!(config.num_operations, 50);
        assert_eq!(config.server_weights.drop_promise, 0);
    }

    #[test]
    fn test_multi_node_client_config_default() {
        let config = MultiNodeClientConfig::default();
        assert_eq!(config.num_operations, 100);
    }

    #[test]
    fn test_multi_node_client_config_happy_path() {
        let config = MultiNodeClientConfig::happy_path();
        assert_eq!(config.num_operations, 50);
    }
}
*/
