//! Workload implementations for simulation testing.
//!
//! Provides client and server workloads that exercise NetTransport,
//! EndpointMap, and NetNotifiedQueue under chaos conditions.

#![allow(dead_code)] // Config fields may not all be used in every workload

use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::time::Duration;

use crate::{
    Endpoint, JsonCodec, MessageReceiver, NetNotifiedQueue, NetTransportBuilder, NetworkAddress,
    ReplyPromise, TimeProvider, UID, send_request,
};
use async_trait::async_trait;
use moonpool_sim::{SimContext, SimulationResult, Workload, assert_sometimes};

use super::invariants::{MessageInvariants, RpcInvariants};
use super::operations::{
    ClientOp, ClientOpWeights, RpcClientOp, RpcClientOpWeights, RpcServerOp, RpcServerOpWeights,
    generate_client_op, generate_rpc_client_op, generate_rpc_server_op,
};
use super::{RpcTestRequest, RpcTestResponse, TestMessage};

// ============================================================================
// Local Delivery Workload
// ============================================================================

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
pub struct LocalDeliveryWorkload {
    config: LocalDeliveryConfig,
}

impl LocalDeliveryWorkload {
    /// Create a new local delivery workload.
    pub fn new(config: LocalDeliveryConfig) -> Self {
        Self { config }
    }
}

#[async_trait(?Send)]
impl Workload for LocalDeliveryWorkload {
    fn name(&self) -> &str {
        "local_delivery"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let my_id = ctx.my_ip().to_string();
        let local_addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500);

        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(local_addr.clone())
            .build()
            .expect("build should succeed");

        // Create message queue and register with transport
        let token = UID::new(0x1234, 0x5678);
        let queue: Rc<NetNotifiedQueue<TestMessage, JsonCodec>> = Rc::new(NetNotifiedQueue::new(
            Endpoint::new(local_addr.clone(), token),
            JsonCodec,
        ));
        let endpoint = transport.register(token, queue.clone() as Rc<dyn MessageReceiver>);

        let invariants = Rc::new(RefCell::new(MessageInvariants::new()));
        let mut next_reliable_seq = 0u64;
        let mut next_unreliable_seq = 10000u64;

        for _op_num in 0..self.config.num_operations {
            let op = generate_client_op(
                ctx.random(),
                &mut next_reliable_seq,
                &mut next_unreliable_seq,
                &self.config.weights,
            );

            match op {
                ClientOp::SendReliable {
                    seq_id,
                    payload_size,
                } => {
                    let msg = TestMessage::with_payload(seq_id, &my_id, true, payload_size);
                    let payload = msg.to_bytes();
                    if transport.send_reliable(&endpoint, &payload).is_ok() {
                        invariants.borrow_mut().record_sent(seq_id, true);
                    }
                }
                ClientOp::SendUnreliable {
                    seq_id,
                    payload_size,
                } => {
                    let msg = TestMessage::with_payload(seq_id, &my_id, false, payload_size);
                    let payload = msg.to_bytes();
                    if transport.send_unreliable(&endpoint, &payload).is_ok() {
                        invariants.borrow_mut().record_sent(seq_id, false);
                    }
                }
                ClientOp::SmallDelay => {
                    let _ = ctx.time().sleep(Duration::from_millis(1)).await;
                }
            }

            // Drain the queue and validate
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

        invariants.borrow().validate_always();

        // Publish for invariant checker
        ctx.state()
            .publish("message_invariants", invariants.borrow().clone());

        Ok(())
    }
}

// ============================================================================
// RPC Workload
// ============================================================================

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
pub struct RpcWorkload {
    config: RpcWorkloadConfig,
}

impl RpcWorkload {
    /// Create a new RPC workload.
    pub fn new(config: RpcWorkloadConfig) -> Self {
        Self { config }
    }
}

#[async_trait(?Send)]
impl Workload for RpcWorkload {
    fn name(&self) -> &str {
        "rpc"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let my_id = ctx.my_ip().to_string();
        let local_addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500);

        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(local_addr.clone())
            .build()
            .expect("build should succeed");

        let server_token = UID::new(0xAABB, 0xCCDD);
        let request_stream =
            transport.register_handler::<RpcTestRequest, _>(server_token, JsonCodec);
        let server_endpoint = request_stream.endpoint().clone();

        let invariants = Rc::new(RefCell::new(RpcInvariants::new()));
        let mut next_request_id = 0u64;
        let mut pending_requests: Vec<u64> = Vec::new();
        let mut pending_server_requests: Vec<(u64, ReplyPromise<RpcTestResponse, JsonCodec>)> =
            Vec::new();

        for _op_num in 0..self.config.num_operations {
            // Client operation
            let client_op = generate_rpc_client_op(
                ctx.random(),
                &mut next_request_id,
                &pending_requests,
                &self.config.client_weights,
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
                            assert_sometimes!(true, "Should be able to send RPC requests");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "RPC request send failed");
                            invariants.borrow_mut().record_connection_failure();
                        }
                    }
                }
                RpcClientOp::AwaitResponse { request_id } => {
                    pending_requests.retain(|&id| id != request_id);
                }
                RpcClientOp::AwaitWithTimeout {
                    request_id,
                    timeout_ms,
                } => {
                    let timeout_duration = Duration::from_millis(timeout_ms);
                    let server_has_pending = pending_server_requests
                        .iter()
                        .any(|(id, _)| *id == request_id);
                    if server_has_pending {
                        let _ = ctx.time().sleep(timeout_duration).await;
                    }
                    pending_requests.retain(|&id| id != request_id);
                }
                RpcClientOp::SmallDelay => {
                    let _ = ctx.time().sleep(Duration::from_millis(1)).await;
                }
            }

            // Server operation
            let pending_ids: Vec<u64> = pending_server_requests.iter().map(|(id, _)| *id).collect();
            let server_op =
                generate_rpc_server_op(ctx.random(), &pending_ids, &self.config.server_weights);

            match server_op {
                RpcServerOp::TryReceiveRequest => {
                    if let Some((request, reply)) =
                        request_stream.try_recv_with_transport::<_, RpcTestResponse>(&transport)
                    {
                        assert_sometimes!(true, "Server should receive RPC requests");
                        pending_server_requests.push((request.request_id, reply));
                    }
                }
                RpcServerOp::SendResponse { request_id } => {
                    if let Some(pos) = pending_server_requests
                        .iter()
                        .position(|(id, _)| *id == request_id)
                    {
                        let (req_id, reply) = pending_server_requests.remove(pos);
                        reply.send(RpcTestResponse::success(req_id));
                        invariants.borrow_mut().record_response_received(req_id);
                        assert_sometimes!(true, "Server should send RPC responses");
                    }
                }
                RpcServerOp::DropPromise { request_id } => {
                    if let Some(pos) = pending_server_requests
                        .iter()
                        .position(|(id, _)| *id == request_id)
                    {
                        let (req_id, reply) = pending_server_requests.remove(pos);
                        drop(reply);
                        invariants.borrow_mut().record_broken_promise(req_id);
                        assert_sometimes!(true, "Server should sometimes drop promises");
                    }
                }
                RpcServerOp::SmallDelay => {
                    let _ = ctx.time().sleep(Duration::from_millis(1)).await;
                }
            }

            invariants.borrow().validate_always();
        }

        // Drain remaining
        while let Some((request, reply)) =
            request_stream.try_recv_with_transport::<_, RpcTestResponse>(&transport)
        {
            pending_server_requests.push((request.request_id, reply));
        }

        for (req_id, reply) in pending_server_requests.drain(..) {
            reply.send(RpcTestResponse::success(req_id));
            invariants.borrow_mut().record_response_received(req_id);
        }

        let inv = invariants.borrow();
        inv.validate_always();
        inv.validate_coverage();
        tracing::info!(summary = %inv.summary(), "RPC workload completed");

        // Publish for invariant checker
        ctx.state().publish("rpc_invariants", inv.clone());

        Ok(())
    }
}

// ============================================================================
// Multi-Node RPC Workloads
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

/// Multi-node RPC server workload.
pub struct MultiNodeServerWorkload {
    config: MultiNodeServerConfig,
}

impl MultiNodeServerWorkload {
    /// Create a new multi-node server workload.
    pub fn new(config: MultiNodeServerConfig) -> Self {
        Self { config }
    }
}

#[async_trait(?Send)]
impl Workload for MultiNodeServerWorkload {
    fn name(&self) -> &str {
        "server"
    }

    async fn setup(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let my_id = ctx.my_ip().to_string();
        tracing::info!(server_id = %my_id, "Starting multi-node RPC server");

        let addr_with_port = if my_id.contains(':') {
            my_id.clone()
        } else {
            format!("{}:4500", my_id)
        };
        let local_addr =
            NetworkAddress::parse(&addr_with_port).expect("valid server address in topology");

        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(local_addr.clone())
            .build_listening()
            .await
            .expect("server listen failed");

        tracing::info!(addr = %local_addr, "Server listening");
        assert_sometimes!(true, "Server should start listening");

        let server_token = UID::new(0xAABB, 0xCCDD);
        let request_stream =
            transport.register_handler::<RpcTestRequest, _>(server_token, JsonCodec);

        let invariants = Rc::new(RefCell::new(RpcInvariants::new()));
        let mut pending_server_requests: Vec<(u64, ReplyPromise<RpcTestResponse, JsonCodec>)> =
            Vec::new();

        for _op_num in 0..self.config.num_operations {
            let pending_ids: Vec<u64> = pending_server_requests.iter().map(|(id, _)| *id).collect();
            let server_op =
                generate_rpc_server_op(ctx.random(), &pending_ids, &self.config.server_weights);

            match server_op {
                RpcServerOp::TryReceiveRequest => {
                    if let Some((request, reply)) =
                        request_stream.try_recv_with_transport::<_, RpcTestResponse>(&transport)
                    {
                        assert_sometimes!(true, "Server should receive RPC requests from network");
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
                        reply.send(RpcTestResponse::success(req_id));
                        invariants.borrow_mut().record_response_received(req_id);
                        assert_sometimes!(true, "Server should send RPC responses");
                        tracing::debug!(request_id = req_id, "Server sent response");
                    }
                }
                RpcServerOp::DropPromise { request_id } => {
                    if let Some(pos) = pending_server_requests
                        .iter()
                        .position(|(id, _)| *id == request_id)
                    {
                        let (req_id, reply) = pending_server_requests.remove(pos);
                        drop(reply);
                        invariants.borrow_mut().record_broken_promise(req_id);
                        assert_sometimes!(true, "Server should sometimes drop promises");
                    }
                }
                RpcServerOp::SmallDelay => {
                    let _ = ctx.time().sleep(Duration::from_millis(1)).await;
                }
            }

            // Publish state for cross-node validation
            ctx.state()
                .publish("rpc_invariants", invariants.borrow().clone());
        }

        // Drain remaining
        while let Some((request, reply)) =
            request_stream.try_recv_with_transport::<_, RpcTestResponse>(&transport)
        {
            pending_server_requests.push((request.request_id, reply));
        }

        for (req_id, reply) in pending_server_requests.drain(..) {
            reply.send(RpcTestResponse::success(req_id));
            invariants.borrow_mut().record_response_received(req_id);
        }

        ctx.state()
            .publish("rpc_invariants", invariants.borrow().clone());

        tracing::info!(summary = %invariants.borrow().summary(), "Multi-node server completed");
        Ok(())
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

/// Multi-node RPC client workload.
pub struct MultiNodeClientWorkload {
    config: MultiNodeClientConfig,
}

impl MultiNodeClientWorkload {
    /// Create a new multi-node client workload.
    pub fn new(config: MultiNodeClientConfig) -> Self {
        Self { config }
    }
}

#[async_trait(?Send)]
impl Workload for MultiNodeClientWorkload {
    fn name(&self) -> &str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let my_id = ctx.my_ip().to_string();
        tracing::info!(client_id = %my_id, "Starting multi-node RPC client");

        let addr_with_port = if my_id.contains(':') {
            my_id.clone()
        } else {
            format!("{}:4501", my_id)
        };
        let local_addr =
            NetworkAddress::parse(&addr_with_port).expect("valid client address in topology");

        // Find server peer
        let server_addr_str = ctx
            .peer("server")
            .or_else(|| ctx.peers().first().map(|(_, ip)| ip.clone()))
            .expect("server address in topology");

        let server_addr_with_port = if server_addr_str.contains(':') {
            server_addr_str.clone()
        } else {
            format!("{}:4500", server_addr_str)
        };
        let server_addr =
            NetworkAddress::parse(&server_addr_with_port).expect("valid server address");

        tracing::info!(server = %server_addr, "Client connecting to server");

        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(local_addr.clone())
            .build()
            .expect("build should succeed");

        let server_token = UID::new(0xAABB, 0xCCDD);
        let server_endpoint = Endpoint::new(server_addr.clone(), server_token);

        let invariants = Rc::new(RefCell::new(RpcInvariants::new()));
        let mut next_request_id = 0u64;
        let mut pending_requests: Vec<u64> = Vec::new();

        for _op_num in 0..self.config.num_operations {
            let client_op = generate_rpc_client_op(
                ctx.random(),
                &mut next_request_id,
                &pending_requests,
                &self.config.client_weights,
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
                            assert_sometimes!(true, "Client should send RPC requests over network");
                            tracing::debug!(request_id, "Client sent request");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "RPC request send failed");
                            invariants.borrow_mut().record_connection_failure();
                        }
                    }
                }
                RpcClientOp::AwaitResponse { request_id } => {
                    pending_requests.retain(|&id| id != request_id);
                }
                RpcClientOp::AwaitWithTimeout {
                    request_id,
                    timeout_ms,
                } => {
                    let timeout_duration = Duration::from_millis(timeout_ms);
                    let _ = ctx.time().sleep(timeout_duration).await;
                    pending_requests.retain(|&id| id != request_id);
                }
                RpcClientOp::SmallDelay => {
                    let _ = ctx.time().sleep(Duration::from_millis(1)).await;
                }
            }

            invariants.borrow().validate_always();
        }

        tracing::info!(summary = %invariants.borrow().summary(), "Multi-node client completed");
        Ok(())
    }
}

// ============================================================================
// Multi-Method Workload
// ============================================================================

use super::{
    CalcAddRequest, CalcAddResponse, CalcMulRequest, CalcMulResponse, CalcSubRequest,
    CalcSubResponse, calculator,
};
use moonpool_sim::RandomProvider;

/// Multi-method interface workload for testing register_handler_at().
pub struct MultiMethodWorkload {
    num_operations: u64,
}

impl MultiMethodWorkload {
    /// Create a new multi-method workload.
    pub fn new(num_operations: u64) -> Self {
        Self { num_operations }
    }
}

#[async_trait(?Send)]
impl Workload for MultiMethodWorkload {
    fn name(&self) -> &str {
        "multi_method"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let local_addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500);

        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(local_addr.clone())
            .build()
            .expect("build should succeed");

        let (add_stream, add_token) = transport.register_handler_at::<CalcAddRequest, _>(
            calculator::CALC_INTERFACE,
            calculator::METHOD_ADD,
            JsonCodec,
        );
        let (sub_stream, sub_token) = transport.register_handler_at::<CalcSubRequest, _>(
            calculator::CALC_INTERFACE,
            calculator::METHOD_SUB,
            JsonCodec,
        );
        let (mul_stream, mul_token) = transport.register_handler_at::<CalcMulRequest, _>(
            calculator::CALC_INTERFACE,
            calculator::METHOD_MUL,
            JsonCodec,
        );

        let add_endpoint = Endpoint::new(local_addr.clone(), add_token);
        let sub_endpoint = Endpoint::new(local_addr.clone(), sub_token);
        let mul_endpoint = Endpoint::new(local_addr.clone(), mul_token);

        let mut pending_add: Vec<(u64, ReplyPromise<CalcAddResponse, JsonCodec>)> = Vec::new();
        let mut pending_sub: Vec<(u64, ReplyPromise<CalcSubResponse, JsonCodec>)> = Vec::new();
        let mut pending_mul: Vec<(u64, ReplyPromise<CalcMulResponse, JsonCodec>)> = Vec::new();

        let mut next_request_id = 0u64;
        let mut add_count = 0u64;
        let mut sub_count = 0u64;
        let mut mul_count = 0u64;

        for _op_num in 0..self.num_operations {
            let method_choice = ctx.random().random_range(0..3);

            match method_choice {
                0 => {
                    let request_id = next_request_id;
                    next_request_id += 1;
                    let a = ctx.random().random_range(0..100) as i64;
                    let b = ctx.random().random_range(0..100) as i64;
                    let request = CalcAddRequest { a, b, request_id };
                    if send_request::<_, CalcAddResponse, _, _>(
                        &transport,
                        &add_endpoint,
                        request,
                        JsonCodec,
                    )
                    .is_ok()
                    {
                        add_count += 1;
                        assert_sometimes!(true, "Should send ADD requests");
                    }
                }
                1 => {
                    let request_id = next_request_id;
                    next_request_id += 1;
                    let a = ctx.random().random_range(0..100) as i64;
                    let b = ctx.random().random_range(0..100) as i64;
                    let request = CalcSubRequest { a, b, request_id };
                    if send_request::<_, CalcSubResponse, _, _>(
                        &transport,
                        &sub_endpoint,
                        request,
                        JsonCodec,
                    )
                    .is_ok()
                    {
                        sub_count += 1;
                        assert_sometimes!(true, "Should send SUB requests");
                    }
                }
                _ => {
                    let request_id = next_request_id;
                    next_request_id += 1;
                    let a = ctx.random().random_range(0..100) as i64;
                    let b = ctx.random().random_range(0..100) as i64;
                    let request = CalcMulRequest { a, b, request_id };
                    if send_request::<_, CalcMulResponse, _, _>(
                        &transport,
                        &mul_endpoint,
                        request,
                        JsonCodec,
                    )
                    .is_ok()
                    {
                        mul_count += 1;
                        assert_sometimes!(true, "Should send MUL requests");
                    }
                }
            }

            // Process received requests
            if let Some((request, reply)) =
                add_stream.try_recv_with_transport::<_, CalcAddResponse>(&transport)
            {
                pending_add.push((request.request_id, reply));
                assert_sometimes!(true, "Should receive ADD requests via correct handler");
            }
            if let Some((request, reply)) =
                sub_stream.try_recv_with_transport::<_, CalcSubResponse>(&transport)
            {
                pending_sub.push((request.request_id, reply));
                assert_sometimes!(true, "Should receive SUB requests via correct handler");
            }
            if let Some((request, reply)) =
                mul_stream.try_recv_with_transport::<_, CalcMulResponse>(&transport)
            {
                pending_mul.push((request.request_id, reply));
                assert_sometimes!(true, "Should receive MUL requests via correct handler");
            }

            // Respond to some pending requests
            if !pending_add.is_empty() && ctx.random().random_range(0..2) == 0 {
                let (req_id, reply) = pending_add.remove(0);
                reply.send(CalcAddResponse {
                    result: 42,
                    request_id: req_id,
                });
            }
            if !pending_sub.is_empty() && ctx.random().random_range(0..2) == 0 {
                let (req_id, reply) = pending_sub.remove(0);
                reply.send(CalcSubResponse {
                    result: 0,
                    request_id: req_id,
                });
            }
            if !pending_mul.is_empty() && ctx.random().random_range(0..2) == 0 {
                let (req_id, reply) = pending_mul.remove(0);
                reply.send(CalcMulResponse {
                    result: 1,
                    request_id: req_id,
                });
            }

            let _ = ctx.time().sleep(Duration::from_millis(1)).await;
        }

        // Drain all pending
        while let Some((request, reply)) =
            add_stream.try_recv_with_transport::<_, CalcAddResponse>(&transport)
        {
            reply.send(CalcAddResponse {
                result: 0,
                request_id: request.request_id,
            });
        }
        while let Some((request, reply)) =
            sub_stream.try_recv_with_transport::<_, CalcSubResponse>(&transport)
        {
            reply.send(CalcSubResponse {
                result: 0,
                request_id: request.request_id,
            });
        }
        while let Some((request, reply)) =
            mul_stream.try_recv_with_transport::<_, CalcMulResponse>(&transport)
        {
            reply.send(CalcMulResponse {
                result: 0,
                request_id: request.request_id,
            });
        }

        for (req_id, reply) in pending_add {
            reply.send(CalcAddResponse {
                result: 0,
                request_id: req_id,
            });
        }
        for (req_id, reply) in pending_sub {
            reply.send(CalcSubResponse {
                result: 0,
                request_id: req_id,
            });
        }
        for (req_id, reply) in pending_mul {
            reply.send(CalcMulResponse {
                result: 0,
                request_id: req_id,
            });
        }

        tracing::info!(
            add = add_count,
            sub = sub_count,
            mul = mul_count,
            "Multi-method workload completed"
        );
        Ok(())
    }
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
