//! Node service and workload.
//!
//! The Node acts as a hypervisor: polls requests from the broker, processes
//! VM commands, and sends responses back through the broker.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use async_trait::async_trait;
use moonpool_sim::{SimContext, SimulationResult, Workload};
use moonpool_transport::{
    JsonCodec, NetTransport, NetTransportBuilder, NetworkAddress, RpcError, ServerHandle, service,
};
use serde::{Deserialize, Serialize};

use super::broker::{
    BoundMessageBrokerClient, MessageBroker, MessageBrokerClient, PollRequest, ProduceRequest,
};
use super::driver::BrokerMessage;

// ============================================================================
// Message Types
// ============================================================================

/// Request to poll and process pending commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollAndProcessRequest;

/// Response from poll and process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollAndProcessResponse {
    /// Number of commands processed.
    pub processed: usize,
}

/// Request to list VMs on the node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeListVmsRequest;

/// Response with the node's VM list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeListVmsResponse {
    /// VM names running on the node.
    pub vms: Vec<String>,
}

// ============================================================================
// Service Definition
// ============================================================================

/// Node RPC interface.
///
/// Generates `NodeServer<C>`, `NodeClient`, `BoundNodeClient`.
#[service(id = 0x40DE_4E00)]
pub trait Node {
    /// Poll the broker for requests, process them, and send responses.
    async fn poll_and_process(
        &self,
        req: PollAndProcessRequest,
    ) -> Result<PollAndProcessResponse, RpcError>;

    /// List all VMs running on the node.
    async fn list_vms(&self, req: NodeListVmsRequest) -> Result<NodeListVmsResponse, RpcError>;
}

// ============================================================================
// Handler
// ============================================================================

/// Node handler with VM set and broker client.
pub struct NodeHandler {
    /// Running VMs on this node.
    vms: Rc<RefCell<HashSet<String>>>,
    /// Client to the MessageBroker.
    broker: Rc<BoundMessageBrokerClient<moonpool_sim::SimProviders, JsonCodec>>,
}

impl NodeHandler {
    /// Create a new handler with an empty VM set.
    pub fn new(
        broker: Rc<BoundMessageBrokerClient<moonpool_sim::SimProviders, JsonCodec>>,
    ) -> Self {
        Self {
            vms: Rc::new(RefCell::new(HashSet::new())),
            broker,
        }
    }
}

#[async_trait(?Send)]
impl Node for NodeHandler {
    async fn poll_and_process(
        &self,
        _req: PollAndProcessRequest,
    ) -> Result<PollAndProcessResponse, RpcError> {
        let mut processed = 0;

        // Poll all available requests
        loop {
            match self
                .broker
                .poll(PollRequest {
                    topic: "requests".to_string(),
                })
                .await
            {
                Ok(super::broker::PollResponse { message: Some(msg) }) => {
                    match msg {
                        BrokerMessage::CreateVm { ref name } => {
                            self.vms.borrow_mut().insert(name.clone());
                            // Send response back through broker (ignore errors)
                            let _ = self
                                .broker
                                .produce(ProduceRequest {
                                    topic: "responses".to_string(),
                                    message: BrokerMessage::VmCreated { name: name.clone() },
                                })
                                .await;
                        }
                        BrokerMessage::DeleteVm { ref name } => {
                            self.vms.borrow_mut().remove(name);
                            let _ = self
                                .broker
                                .produce(ProduceRequest {
                                    topic: "responses".to_string(),
                                    message: BrokerMessage::VmDeleted { name: name.clone() },
                                })
                                .await;
                        }
                        _ => {}
                    }
                    processed += 1;
                }
                Ok(super::broker::PollResponse { message: None }) => break,
                Err(_) => break, // Broker error — stop polling
            }
        }

        Ok(PollAndProcessResponse { processed })
    }

    async fn list_vms(&self, _req: NodeListVmsRequest) -> Result<NodeListVmsResponse, RpcError> {
        let vms: Vec<String> = self.vms.borrow().iter().cloned().collect();
        Ok(NodeListVmsResponse { vms })
    }
}

// ============================================================================
// Workload
// ============================================================================

/// Workload that hosts the Node RPC server.
pub struct NodeWorkload {
    transport: Option<Rc<NetTransport<moonpool_sim::SimProviders>>>,
    server_handle: Option<ServerHandle>,
}

impl NodeWorkload {
    /// Create a new node workload.
    pub fn new() -> Self {
        Self {
            transport: None,
            server_handle: None,
        }
    }
}

#[async_trait(?Send)]
impl Workload for NodeWorkload {
    fn name(&self) -> &str {
        "node"
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

        // Connect to broker
        let broker_ip = ctx.peer("broker").ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("broker not found".into())
        })?;
        let broker_addr = NetworkAddress::parse(&format!("{broker_ip}:4500")).map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("bad broker address: {e}"))
        })?;
        let broker_client =
            MessageBrokerClient::new(broker_addr).bind(transport.clone(), JsonCodec);

        let server = NodeServer::init(&transport, JsonCodec);
        let handler = Rc::new(NodeHandler::new(Rc::new(broker_client)));
        let handle = server.serve(transport.clone(), handler, ctx.providers());

        self.transport = Some(transport);
        self.server_handle = Some(handle);

        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.shutdown().cancelled().await;
        self.server_handle.take();
        Ok(())
    }
}
