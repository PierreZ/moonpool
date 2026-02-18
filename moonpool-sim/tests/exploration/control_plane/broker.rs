//! MessageBroker service and workload.
//!
//! The broker stores messages in topic-keyed queues. It has an injected bug:
//! with P=0.10 probability, operations succeed internally but return errors,
//! creating consistency divergence between ControlPlane and Node.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use async_trait::async_trait;
use moonpool_sim::{SimContext, SimulationResult, Workload};
use moonpool_transport::{
    JsonCodec, NetTransport, NetTransportBuilder, NetworkAddress, RpcError, ServerHandle, service,
};
use serde::{Deserialize, Serialize};

use super::driver::BrokerMessage;

// ============================================================================
// Message Types
// ============================================================================

/// Request to produce a message to a topic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProduceRequest {
    /// Topic name (e.g., "requests", "responses").
    pub topic: String,
    /// The message to enqueue.
    pub message: BrokerMessage,
}

/// Response from a produce operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProduceResponse;

/// Request to poll a message from a topic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollRequest {
    /// Topic name to poll from.
    pub topic: String,
}

/// Response from a poll operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollResponse {
    /// The dequeued message, if any.
    pub message: Option<BrokerMessage>,
}

// ============================================================================
// Service Definition
// ============================================================================

/// MessageBroker RPC interface.
///
/// Generates `MessageBrokerServer<C>`, `MessageBrokerClient`, `BoundMessageBrokerClient`.
#[service(id = 0xBE0C_4E00)]
pub trait MessageBroker {
    /// Produce a message to a topic.
    async fn produce(&self, req: ProduceRequest) -> Result<ProduceResponse, RpcError>;

    /// Poll (dequeue) a message from a topic.
    async fn poll(&self, req: PollRequest) -> Result<PollResponse, RpcError>;
}

// ============================================================================
// Handler
// ============================================================================

/// Bug trigger probability per operation.
const BUG_PROBABILITY: f64 = 0.10;

/// MessageBroker handler with injected bug.
pub struct MessageBrokerHandler {
    topics: Rc<RefCell<HashMap<String, VecDeque<BrokerMessage>>>>,
}

impl MessageBrokerHandler {
    /// Create a new handler with empty topic queues.
    pub fn new() -> Self {
        Self {
            topics: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

#[async_trait(?Send)]
impl MessageBroker for MessageBrokerHandler {
    async fn produce(&self, req: ProduceRequest) -> Result<ProduceResponse, RpcError> {
        // Always store the message internally
        self.topics
            .borrow_mut()
            .entry(req.topic)
            .or_default()
            .push_back(req.message);

        // Bug: with P=0.10, return error despite internal success
        if moonpool_sim::sim_random::<f64>() < BUG_PROBABILITY {
            moonpool_sim::assert_sometimes!(true, "broker_produce_bug_triggered");
            return Err(RpcError::Reply(
                moonpool_transport::ReplyError::BrokenPromise,
            ));
        }

        Ok(ProduceResponse)
    }

    async fn poll(&self, req: PollRequest) -> Result<PollResponse, RpcError> {
        // Always consume the message internally
        let message = {
            let mut topics = self.topics.borrow_mut();
            topics
                .get_mut(&req.topic)
                .and_then(|q: &mut VecDeque<BrokerMessage>| q.pop_front())
        };

        // Bug: with P=0.10, return error despite internal success
        if message.is_some() && moonpool_sim::sim_random::<f64>() < BUG_PROBABILITY {
            moonpool_sim::assert_sometimes!(true, "broker_poll_bug_triggered");
            return Err(RpcError::Reply(
                moonpool_transport::ReplyError::BrokenPromise,
            ));
        }

        Ok(PollResponse { message })
    }
}

// ============================================================================
// Workload
// ============================================================================

/// Workload that hosts the MessageBroker RPC server.
pub struct BrokerWorkload {
    transport: Option<Rc<NetTransport<moonpool_sim::SimProviders>>>,
    server_handle: Option<ServerHandle>,
}

impl BrokerWorkload {
    /// Create a new broker workload.
    pub fn new() -> Self {
        Self {
            transport: None,
            server_handle: None,
        }
    }
}

#[async_trait(?Send)]
impl Workload for BrokerWorkload {
    fn name(&self) -> &str {
        "broker"
    }

    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let local_addr = NetworkAddress::parse(&format!("{}:4500", ctx.my_ip())).map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("bad address: {e}"))
        })?;

        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(local_addr)
            .build_listening()
            .await
            .map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("transport build: {e}"))
            })?;

        let server = MessageBrokerServer::init(&transport, JsonCodec);
        let handler = Rc::new(MessageBrokerHandler::new());
        let handle = server.serve(transport.clone(), handler, ctx.providers());

        self.transport = Some(transport);
        self.server_handle = Some(handle);

        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.shutdown().cancelled().await;
        // Drop server handle for clean shutdown
        self.server_handle.take();
        Ok(())
    }
}
