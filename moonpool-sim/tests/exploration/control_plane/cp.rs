//! ControlPlane service and workload.
//!
//! Maintains a registry of known VMs. Produces requests to the broker and
//! polls responses to confirm operations. Returns domain results, never
//! propagating broker RPC errors directly.

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
    BoundMessageBrokerClient, MessageBroker, MessageBrokerClient, PollRequest, PollResponse,
    ProduceRequest,
};
use super::driver::BrokerMessage;

// ============================================================================
// Message Types
// ============================================================================

/// Request to create a VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateVmRequest {
    /// Name of the VM to create.
    pub name: String,
}

/// Response from a create VM operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateVmResponse {
    /// Whether the produce to broker succeeded.
    pub success: bool,
}

/// Request to delete a VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteVmRequest {
    /// Name of the VM to delete.
    pub name: String,
}

/// Response from a delete VM operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteVmResponse {
    /// Whether the produce to broker succeeded.
    pub success: bool,
}

/// Request to list known VMs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListVmsRequest;

/// Response with the list of known VMs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListVmsResponse {
    /// VM names in the registry.
    pub vms: Vec<String>,
}

/// Request to poll responses from the broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollResponsesRequest;

/// Response from polling broker responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollResponsesResponse {
    /// Number of responses processed.
    pub processed: usize,
}

// ============================================================================
// Service Definition
// ============================================================================

/// ControlPlane RPC interface.
///
/// Generates `ControlPlaneServer<C>`, `ControlPlaneClient`, `BoundControlPlaneClient`.
#[service(id = 0xC714_4E00)]
pub trait ControlPlane {
    /// Submit a VM creation request to the broker.
    async fn create_vm(&self, req: CreateVmRequest) -> Result<CreateVmResponse, RpcError>;

    /// Submit a VM deletion request to the broker.
    async fn delete_vm(&self, req: DeleteVmRequest) -> Result<DeleteVmResponse, RpcError>;

    /// List all VMs in the registry.
    async fn list_vms(&self, req: ListVmsRequest) -> Result<ListVmsResponse, RpcError>;

    /// Poll the broker for responses and update the registry.
    async fn poll_responses(
        &self,
        req: PollResponsesRequest,
    ) -> Result<PollResponsesResponse, RpcError>;
}

// ============================================================================
// Handler
// ============================================================================

/// ControlPlane handler with VM registry and broker client.
pub struct ControlPlaneHandler {
    /// Known VM names (updated only on confirmed responses).
    registry: Rc<RefCell<HashSet<String>>>,
    /// Client to the MessageBroker.
    broker: Rc<BoundMessageBrokerClient<moonpool_sim::SimProviders, JsonCodec>>,
}

impl ControlPlaneHandler {
    /// Create a new handler with an empty registry.
    pub fn new(
        broker: Rc<BoundMessageBrokerClient<moonpool_sim::SimProviders, JsonCodec>>,
    ) -> Self {
        Self {
            registry: Rc::new(RefCell::new(HashSet::new())),
            broker,
        }
    }
}

#[async_trait(?Send)]
impl ControlPlane for ControlPlaneHandler {
    async fn create_vm(&self, req: CreateVmRequest) -> Result<CreateVmResponse, RpcError> {
        // Produce to broker — do NOT update registry yet (wait for confirmation)
        match self
            .broker
            .produce(ProduceRequest {
                topic: "requests".to_string(),
                message: BrokerMessage::CreateVm { name: req.name },
            })
            .await
        {
            Ok(_) => Ok(CreateVmResponse { success: true }),
            Err(_) => Ok(CreateVmResponse { success: false }),
        }
    }

    async fn delete_vm(&self, req: DeleteVmRequest) -> Result<DeleteVmResponse, RpcError> {
        match self
            .broker
            .produce(ProduceRequest {
                topic: "requests".to_string(),
                message: BrokerMessage::DeleteVm { name: req.name },
            })
            .await
        {
            Ok(_) => Ok(DeleteVmResponse { success: true }),
            Err(_) => Ok(DeleteVmResponse { success: false }),
        }
    }

    async fn list_vms(&self, _req: ListVmsRequest) -> Result<ListVmsResponse, RpcError> {
        let vms: Vec<String> = self.registry.borrow().iter().cloned().collect();
        Ok(ListVmsResponse { vms })
    }

    async fn poll_responses(
        &self,
        _req: PollResponsesRequest,
    ) -> Result<PollResponsesResponse, RpcError> {
        let mut processed = 0;

        // Poll all available responses
        loop {
            match self
                .broker
                .poll(PollRequest {
                    topic: "responses".to_string(),
                })
                .await
            {
                Ok(PollResponse { message: Some(msg) }) => {
                    match msg {
                        BrokerMessage::VmCreated { name } => {
                            self.registry.borrow_mut().insert(name);
                        }
                        BrokerMessage::VmDeleted { name } => {
                            self.registry.borrow_mut().remove(&name);
                        }
                        _ => {}
                    }
                    processed += 1;
                }
                Ok(PollResponse { message: None }) => break,
                Err(_) => break, // Broker error on poll — stop polling
            }
        }

        Ok(PollResponsesResponse { processed })
    }
}

// ============================================================================
// Workload
// ============================================================================

/// Workload that hosts the ControlPlane RPC server.
pub struct ControlPlaneWorkload {
    transport: Option<Rc<NetTransport<moonpool_sim::SimProviders>>>,
    server_handle: Option<ServerHandle>,
}

impl ControlPlaneWorkload {
    /// Create a new control plane workload.
    pub fn new() -> Self {
        Self {
            transport: None,
            server_handle: None,
        }
    }
}

#[async_trait(?Send)]
impl Workload for ControlPlaneWorkload {
    fn name(&self) -> &str {
        "control_plane"
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

        let server = ControlPlaneServer::init(&transport, JsonCodec);
        let handler = Rc::new(ControlPlaneHandler::new(Rc::new(broker_client)));
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
