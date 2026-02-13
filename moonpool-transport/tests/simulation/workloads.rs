//! FDB-style simulation workloads for transport testing.
//!
//! Implements the Workload trait with setup/run/check lifecycle:
//! - `LocalDeliveryWorkload`: Single-node alphabet-driven testing
//! - `ServerWorkload` + `ClientWorkload`: Multi-node RPC testing

use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    JsonCodec, NetworkAddress, RandomProvider, SimProviders, SimulationError, TimeProvider, UID,
};
use moonpool_transport::{NetTransportBuilder, ReplyError, RequestStream, send_request};

use super::reference_model::TransportRefModel;
use super::{RpcTestRequest, RpcTestResponse, TestMessage};
use crate::simulation::alphabet::{AlphabetWeights, OpCounters, TransportOp, pick_operation};

/// State key for publishing the reference model to SharedState.
const REF_MODEL_KEY: &str = "transport_ref_model";

/// Number of operations per workload run.
const DEFAULT_OPS_COUNT: usize = 50;

/// Helper to parse an IP string into a NetworkAddress.
fn parse_address(ip: &str, port: u16) -> NetworkAddress {
    let ip_addr: IpAddr = ip.parse().unwrap_or(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
    NetworkAddress::new(ip_addr, port)
}

// ============================================================================
// LocalDeliveryWorkload
// ============================================================================

/// Single-node workload that tests local message delivery and RPC.
///
/// Replaces the old local delivery, endpoint, and RPC local workloads
/// with a unified alphabet-driven approach.
pub struct LocalDeliveryWorkload {
    /// Alphabet weights controlling operation distribution.
    weights: AlphabetWeights,
    /// Number of operations to execute per run.
    ops_count: usize,
}

impl LocalDeliveryWorkload {
    /// Create a new local delivery workload with default settings.
    pub fn new() -> Self {
        Self {
            weights: AlphabetWeights::default(),
            ops_count: DEFAULT_OPS_COUNT,
        }
    }

    /// Create with specific weights.
    pub fn with_weights(weights: AlphabetWeights) -> Self {
        Self {
            weights,
            ops_count: DEFAULT_OPS_COUNT,
        }
    }

    /// Set the number of operations per run.
    pub fn with_ops_count(mut self, count: usize) -> Self {
        self.ops_count = count;
        self
    }
}

#[async_trait(?Send)]
impl moonpool_sim::Workload for LocalDeliveryWorkload {
    fn name(&self) -> &str {
        "local_delivery"
    }

    async fn run(&self, ctx: &moonpool_sim::SimContext) -> Result<(), SimulationError> {
        let local_addr = parse_address(ctx.my_ip(), 5000);

        // Build and listen
        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(local_addr.clone())
            .build_listening()
            .await
            .map_err(|e| SimulationError::InvalidState(format!("transport build: {e}")))?;

        // Register a message endpoint for reliable/unreliable
        let msg_token = UID::new(1, 100);
        let msg_queue: Rc<moonpool_transport::NetNotifiedQueue<TestMessage, JsonCodec>> =
            Rc::new(moonpool_transport::NetNotifiedQueue::new(
                moonpool_sim::Endpoint::new(local_addr.clone(), msg_token),
                JsonCodec,
            ));
        transport.register(
            msg_token,
            msg_queue.clone() as Rc<dyn moonpool_transport::MessageReceiver>,
        );

        // Register an RPC handler
        let rpc_token = UID::well_known(51);
        let rpc_stream: RequestStream<RpcTestRequest, JsonCodec> =
            transport.register_handler(rpc_token, JsonCodec);
        let rpc_endpoint = rpc_stream.endpoint().clone();

        let mut ref_model = TransportRefModel::new();
        let mut counters = OpCounters::new();
        let local_endpoint = moonpool_sim::Endpoint::new(local_addr.clone(), msg_token);

        for i in 0..self.ops_count {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            // Generate a sim event every few ops to avoid deadlock detection.
            // Local dispatch is synchronous and doesn't create sim events.
            if i % 3 == 0 {
                let _ = ctx.time().sleep(Duration::from_millis(1)).await;
            }

            let op = pick_operation(ctx.random(), &self.weights, &mut counters);

            match op {
                TransportOp::SendReliable {
                    seq_id,
                    payload_size,
                } => {
                    let msg = TestMessage::with_payload(seq_id, "local", true, payload_size);
                    let payload = msg.to_bytes();
                    if transport.send_reliable(&local_endpoint, &payload).is_ok() {
                        ref_model.record_reliable_sent(seq_id, payload_size, "local");
                    }
                }
                TransportOp::SendUnreliable {
                    seq_id,
                    payload_size,
                } => {
                    let msg = TestMessage::with_payload(seq_id, "local", false, payload_size);
                    let payload = msg.to_bytes();
                    if transport.send_unreliable(&local_endpoint, &payload).is_ok() {
                        ref_model.record_unreliable_sent(seq_id, payload_size, "local");
                    }
                }
                TransportOp::SendRpc { request_id } => {
                    let req = RpcTestRequest::with_payload(request_id, "local", 0);
                    ref_model.record_rpc_sent(request_id, "local");

                    match send_request::<_, RpcTestResponse, _, _>(
                        &transport,
                        &rpc_endpoint,
                        req,
                        JsonCodec,
                    ) {
                        Ok(future) => {
                            // Process inline: the server handler is in the same
                            // task, so we check for requests and respond before
                            // awaiting the reply.
                            if let Some((srv_req, reply)) = rpc_stream
                                .try_recv_with_transport::<SimProviders, RpcTestResponse>(
                                    &transport,
                                )
                            {
                                reply.send(RpcTestResponse::success(srv_req.request_id));
                            }

                            // Now the reply should be available immediately
                            match ctx.time().timeout(Duration::from_millis(500), future).await {
                                Ok(Ok(_resp)) => {
                                    ref_model.record_rpc_response(request_id);
                                }
                                Ok(Err(ReplyError::BrokenPromise)) => {
                                    ref_model.record_rpc_broken_promise(request_id);
                                }
                                Ok(Err(ReplyError::ConnectionFailed)) => {
                                    ref_model.record_rpc_connection_failure(request_id);
                                }
                                Ok(Err(ReplyError::Timeout)) => {
                                    ref_model.record_rpc_timeout(request_id);
                                }
                                Ok(Err(_)) => {}
                                Err(_) => {
                                    ref_model.record_rpc_timeout(request_id);
                                }
                            }
                        }
                        Err(_) => {
                            ref_model.record_rpc_connection_failure(request_id);
                        }
                    }
                }
                TransportOp::SmallDelay => {
                    let _ = ctx.time().sleep(Duration::from_millis(1)).await;
                }
                TransportOp::SendEmptyPayload { reliable } => {
                    let endpoint = local_endpoint.clone();
                    if reliable {
                        let _ = transport.send_reliable(&endpoint, &[]);
                    } else {
                        let _ = transport.send_unreliable(&endpoint, &[]);
                    }
                }
                TransportOp::SendMaxSizePayload { reliable } => {
                    let big_payload = vec![0xAB; moonpool_transport::MAX_PAYLOAD_SIZE];
                    let endpoint = local_endpoint.clone();
                    if reliable {
                        let _ = transport.send_reliable(&endpoint, &big_payload);
                    } else {
                        let _ = transport.send_unreliable(&endpoint, &big_payload);
                    }
                }
                TransportOp::SendToUnknownEndpoint => {
                    let bad_endpoint =
                        moonpool_sim::Endpoint::new(local_addr.clone(), UID::new(999, 999));
                    let _ = transport.send_reliable(&bad_endpoint, b"ghost");
                }
                TransportOp::UnregisterEndpoint => {
                    transport.unregister(&msg_token);
                }
                TransportOp::ReregisterEndpoint => {
                    transport.register(
                        msg_token,
                        msg_queue.clone() as Rc<dyn moonpool_transport::MessageReceiver>,
                    );
                }
                TransportOp::DropRpcPromise => {
                    // For local: send request, then drop the promise instead of responding
                    let req = RpcTestRequest::with_payload(0, "local_drop", 0);
                    if let Ok(future) = send_request::<_, RpcTestResponse, _, _>(
                        &transport,
                        &rpc_endpoint,
                        req,
                        JsonCodec,
                    ) {
                        // Receive the request but drop the promise
                        if let Some((_srv_req, _reply)) = rpc_stream
                            .try_recv_with_transport::<SimProviders, RpcTestResponse>(&transport)
                        {
                            // Promise dropped here (BrokenPromise sent automatically)
                        }
                        // Await the reply (should get BrokenPromise)
                        let _ = ctx.time().timeout(Duration::from_millis(100), future).await;
                    }
                }
            }

            // Drain received messages
            while let Some(msg) = msg_queue.try_recv() {
                if msg.reliable {
                    ref_model.record_reliable_received(
                        msg.seq_id,
                        msg.payload.len(),
                        &msg.sender_id,
                    );
                } else {
                    ref_model.record_unreliable_received(
                        msg.seq_id,
                        msg.payload.len(),
                        &msg.sender_id,
                    );
                }
            }

            // Yield to let simulation events process
            tokio::task::yield_now().await;
        }

        // Publish final ref model for invariant checking
        ctx.state().publish(REF_MODEL_KEY, ref_model);

        Ok(())
    }

    async fn check(&self, ctx: &moonpool_sim::SimContext) -> Result<(), SimulationError> {
        if let Some(model) = ctx.state().get::<TransportRefModel>(REF_MODEL_KEY) {
            check_always_invariants(&model);
        }
        Ok(())
    }
}

// ============================================================================
// ServerWorkload
// ============================================================================

/// Server-side workload for multi-node RPC testing.
///
/// Binds a listener, accepts connections, and processes RPC requests.
/// Can optionally drop promises to exercise the BrokenPromise path.
pub struct ServerWorkload {
    /// Whether to randomly drop some promises.
    drop_promise_rate: u32,
}

impl ServerWorkload {
    /// Create a server that always responds.
    pub fn new() -> Self {
        Self {
            drop_promise_rate: 0,
        }
    }

    /// Create a server that drops promises at the given rate (0-100).
    pub fn with_drop_rate(rate: u32) -> Self {
        Self {
            drop_promise_rate: rate,
        }
    }
}

#[async_trait(?Send)]
impl moonpool_sim::Workload for ServerWorkload {
    fn name(&self) -> &str {
        "rpc_server"
    }

    async fn run(&self, ctx: &moonpool_sim::SimContext) -> Result<(), SimulationError> {
        let local_addr = parse_address(ctx.my_ip(), 6000);
        let drop_rate = self.drop_promise_rate;

        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(local_addr)
            .build_listening()
            .await
            .map_err(|e| SimulationError::InvalidState(format!("server build: {e}")))?;

        // Register RPC handler
        let rpc_token = UID::well_known(60);
        let rpc_stream: RequestStream<RpcTestRequest, JsonCodec> =
            transport.register_handler(rpc_token, JsonCodec);

        // Serve requests until shutdown
        loop {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            let recv_result = ctx
                .time()
                .timeout(
                    Duration::from_millis(100),
                    rpc_stream.recv_with_transport::<SimProviders, RpcTestResponse>(&transport),
                )
                .await;

            match recv_result {
                Ok(Some((req, reply))) => {
                    // Randomly drop promises based on rate
                    if drop_rate > 0 && ctx.random().random_range(0..100u32) < drop_rate {
                        moonpool_sim::assert_sometimes!(true, "server_dropped_promise");
                        drop(reply); // Triggers BrokenPromise
                    } else {
                        reply.send(RpcTestResponse::success(req.request_id));
                    }
                }
                Ok(None) => {
                    // Stream closed
                    break;
                }
                Err(_) => {
                    // Timeout - loop and check shutdown
                    continue;
                }
            }

            tokio::task::yield_now().await;
        }

        Ok(())
    }
}

// ============================================================================
// ClientWorkload
// ============================================================================

/// Client-side workload for multi-node RPC testing.
///
/// Sends RPC requests to the server and tracks results in the reference model.
pub struct ClientWorkload {
    /// Alphabet weights for operation selection.
    weights: AlphabetWeights,
    /// Number of operations per run.
    ops_count: usize,
}

impl ClientWorkload {
    /// Create a client with default settings.
    pub fn new() -> Self {
        Self {
            weights: AlphabetWeights::default(),
            ops_count: DEFAULT_OPS_COUNT,
        }
    }

    /// Create with specific weights and operation count.
    pub fn with_config(weights: AlphabetWeights, ops_count: usize) -> Self {
        Self { weights, ops_count }
    }
}

#[async_trait(?Send)]
impl moonpool_sim::Workload for ClientWorkload {
    fn name(&self) -> &str {
        "rpc_client"
    }

    async fn run(&self, ctx: &moonpool_sim::SimContext) -> Result<(), SimulationError> {
        let local_addr = parse_address(ctx.my_ip(), 7000);
        let server_addr = parse_address(ctx.peer(), 6000);

        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(local_addr)
            .build_listening()
            .await
            .map_err(|e| SimulationError::InvalidState(format!("client build: {e}")))?;

        let server_rpc_endpoint =
            moonpool_sim::Endpoint::new(server_addr.clone(), UID::well_known(60));

        let mut ref_model = TransportRefModel::new();
        let mut counters = OpCounters::new();

        for _ in 0..self.ops_count {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            let op = pick_operation(ctx.random(), &self.weights, &mut counters);

            match op {
                TransportOp::SendRpc { request_id } => {
                    let req = RpcTestRequest::with_payload(request_id, "client", 0);
                    ref_model.record_rpc_sent(request_id, "client");

                    match send_request::<_, RpcTestResponse, _, _>(
                        &transport,
                        &server_rpc_endpoint,
                        req,
                        JsonCodec,
                    ) {
                        Ok(future) => {
                            match ctx.time().timeout(Duration::from_millis(500), future).await {
                                Ok(Ok(_resp)) => {
                                    ref_model.record_rpc_response(request_id);
                                }
                                Ok(Err(ReplyError::BrokenPromise)) => {
                                    ref_model.record_rpc_broken_promise(request_id);
                                }
                                Ok(Err(ReplyError::ConnectionFailed)) => {
                                    ref_model.record_rpc_connection_failure(request_id);
                                }
                                Ok(Err(ReplyError::Timeout)) => {
                                    ref_model.record_rpc_timeout(request_id);
                                }
                                Ok(Err(_)) => {}
                                Err(_) => {
                                    ref_model.record_rpc_timeout(request_id);
                                }
                            }
                        }
                        Err(_) => {
                            ref_model.record_rpc_connection_failure(request_id);
                        }
                    }
                }
                TransportOp::SendReliable {
                    seq_id,
                    payload_size,
                } => {
                    let msg = TestMessage::with_payload(seq_id, "client", true, payload_size);
                    let payload = msg.to_bytes();
                    let msg_endpoint =
                        moonpool_sim::Endpoint::new(server_addr.clone(), UID::new(1, 100));
                    if transport.send_reliable(&msg_endpoint, &payload).is_ok() {
                        ref_model.record_reliable_sent(seq_id, payload_size, "client");
                    }
                }
                TransportOp::SendUnreliable {
                    seq_id,
                    payload_size,
                } => {
                    let msg = TestMessage::with_payload(seq_id, "client", false, payload_size);
                    let payload = msg.to_bytes();
                    let msg_endpoint =
                        moonpool_sim::Endpoint::new(server_addr.clone(), UID::new(1, 100));
                    if transport.send_unreliable(&msg_endpoint, &payload).is_ok() {
                        ref_model.record_unreliable_sent(seq_id, payload_size, "client");
                    }
                }
                TransportOp::SmallDelay => {
                    let _ = ctx.time().sleep(Duration::from_millis(1)).await;
                }
                // Adversarial and nemesis ops are less relevant for cross-node client
                _ => {
                    let _ = ctx.time().sleep(Duration::from_millis(1)).await;
                }
            }

            tokio::task::yield_now().await;
        }

        // Publish ref model for check phase
        ctx.state().publish(REF_MODEL_KEY, ref_model);

        Ok(())
    }

    async fn check(&self, ctx: &moonpool_sim::SimContext) -> Result<(), SimulationError> {
        if let Some(model) = ctx.state().get::<TransportRefModel>(REF_MODEL_KEY) {
            check_always_invariants(&model);
        }
        Ok(())
    }
}

// ============================================================================
// Invariant checking functions
// ============================================================================

/// Check always-true invariants against the reference model.
///
/// These must never fail regardless of chaos conditions.
pub fn check_always_invariants(model: &TransportRefModel) {
    // No phantom reliable: received ⊆ sent
    for seq_id in model.reliable_received.keys() {
        moonpool_sim::assert_always!(
            model.reliable_sent.contains_key(seq_id),
            &format!("Phantom reliable message: seq_id={seq_id} received but never sent")
        );
    }

    // No phantom unreliable: received ⊆ sent
    for seq_id in model.unreliable_received.keys() {
        moonpool_sim::assert_always!(
            model.unreliable_sent.contains_key(seq_id),
            &format!("Phantom unreliable message: seq_id={seq_id} received but never sent")
        );
    }

    // Unreliable conservation: |received| <= |sent|
    moonpool_sim::assert_always!(
        model.unreliable_received.len() <= model.unreliable_sent.len(),
        "Unreliable received count exceeds sent count"
    );

    // RPC no phantoms: response_ids ⊆ request_ids
    for request_id in model.rpc_responses_received.keys() {
        moonpool_sim::assert_always!(
            model.rpc_requests_sent.contains_key(request_id),
            &format!("Phantom RPC response: request_id={request_id}")
        );
    }

    // RPC single resolution: responses, broken, timeouts are disjoint
    for request_id in model.rpc_responses_received.keys() {
        moonpool_sim::assert_always!(
            !model.rpc_broken_promises.contains(request_id),
            &format!("RPC {request_id} both responded and broken-promised")
        );
        moonpool_sim::assert_always!(
            !model.rpc_timeouts.contains(request_id),
            &format!("RPC {request_id} both responded and timed out")
        );
    }
    for request_id in &model.rpc_broken_promises {
        moonpool_sim::assert_always!(
            !model.rpc_timeouts.contains(request_id),
            &format!("RPC {request_id} both broken-promised and timed out")
        );
    }
}

/// Check sometimes-true assertions (statistical coverage).
///
/// These should be true in at least some simulation runs.
pub fn check_sometimes_assertions(model: &TransportRefModel) {
    // All reliable delivered (in good conditions)
    if !model.reliable_sent.is_empty() {
        moonpool_sim::assert_sometimes!(
            model.reliable_received.len() == model.reliable_sent.len(),
            "All reliable messages delivered"
        );
    }

    // Some unreliable dropped (under chaos)
    if !model.unreliable_sent.is_empty() {
        moonpool_sim::assert_sometimes!(
            model.unreliable_received.len() < model.unreliable_sent.len(),
            "Some unreliable messages dropped"
        );
        moonpool_sim::assert_sometimes!(
            model.unreliable_received.len() == model.unreliable_sent.len(),
            "All unreliable messages delivered"
        );
    }

    // RPC paths exercised
    if !model.rpc_requests_sent.is_empty() {
        moonpool_sim::assert_sometimes!(
            !model.rpc_responses_received.is_empty(),
            "RPC success path exercised"
        );
        moonpool_sim::assert_sometimes!(
            !model.rpc_broken_promises.is_empty(),
            "RPC broken promise path exercised"
        );
    }
}
