//! Driver workload and shared message types.
//!
//! The driver orchestrates the test scenario: creates/deletes VMs via the
//! ControlPlane, polls the Node, and detects consistency divergence.

use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{SimContext, SimulationResult, Workload};
use moonpool_transport::{
    JsonCodec, NetTransport, NetTransportBuilder, NetworkAddress, Providers, TimeProvider,
};
use serde::{Deserialize, Serialize};

use super::cp::{
    BoundControlPlaneClient, ControlPlane, ControlPlaneClient, CreateVmRequest, DeleteVmRequest,
    ListVmsRequest, PollResponsesRequest,
};
use super::node::{BoundNodeClient, Node, NodeClient, NodeListVmsRequest, PollAndProcessRequest};

// ============================================================================
// Shared Message Types
// ============================================================================

/// Messages exchanged through the broker's topic queues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BrokerMessage {
    /// Request from ControlPlane to create a VM.
    CreateVm { name: String },
    /// Request from ControlPlane to delete a VM.
    DeleteVm { name: String },
    /// Response from Node: VM was created.
    VmCreated { name: String },
    /// Response from Node: VM was deleted.
    VmDeleted { name: String },
}

// ============================================================================
// Workload
// ============================================================================

/// Number of VM operations per test run.
const NUM_OPERATIONS: usize = 20;

/// Workload that drives the control-plane test scenario.
pub struct DriverWorkload {
    transport: Option<Rc<NetTransport<moonpool_sim::SimProviders>>>,
    cp_client: Option<BoundControlPlaneClient<moonpool_sim::SimProviders, JsonCodec>>,
    node_client: Option<BoundNodeClient<moonpool_sim::SimProviders, JsonCodec>>,
}

impl DriverWorkload {
    /// Create a new driver workload.
    pub fn new() -> Self {
        Self {
            transport: None,
            cp_client: None,
            node_client: None,
        }
    }
}

#[async_trait(?Send)]
impl Workload for DriverWorkload {
    fn name(&self) -> &str {
        "driver"
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

        let cp_ip = ctx.peer("control_plane").ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("control_plane not found".into())
        })?;
        let cp_addr = NetworkAddress::parse(&format!("{cp_ip}:4500")).map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("bad cp address: {e}"))
        })?;
        let cp_client = ControlPlaneClient::new(cp_addr).bind(transport.clone(), JsonCodec);

        let node_ip = ctx
            .peer("node")
            .ok_or_else(|| moonpool_sim::SimulationError::InvalidState("node not found".into()))?;
        let node_addr = NetworkAddress::parse(&format!("{node_ip}:4500")).map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("bad node address: {e}"))
        })?;
        let node_client = NodeClient::new(node_addr).bind(transport.clone(), JsonCodec);

        self.transport = Some(transport);
        self.cp_client = Some(cp_client);
        self.node_client = Some(node_client);

        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let cp = self.cp_client.as_ref().ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("cp_client not initialized".into())
        })?;
        let node = self.node_client.as_ref().ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("node_client not initialized".into())
        })?;
        let time = ctx.providers().time().clone();

        let mut submitted_creates: HashSet<String> = HashSet::new();

        for i in 0..NUM_OPERATIONS {
            let do_delete =
                !submitted_creates.is_empty() && moonpool_sim::sim_random::<f64>() < 0.3;

            if do_delete {
                // Pick a random VM to delete
                let names: Vec<String> = submitted_creates.iter().cloned().collect();
                let idx = moonpool_sim::sim_random_range(0..names.len());
                let name = names[idx].clone();

                match time
                    .timeout(
                        Duration::from_secs(5),
                        cp.delete_vm(DeleteVmRequest { name: name.clone() }),
                    )
                    .await
                {
                    Ok(Ok(resp)) => {
                        if resp.success {
                            submitted_creates.remove(&name);
                        }
                    }
                    _ => {
                        // Timeout or RPC error — skip
                    }
                }
            } else {
                // Create a new VM
                let name = format!("vm-{i}");
                match time
                    .timeout(
                        Duration::from_secs(5),
                        cp.create_vm(CreateVmRequest { name: name.clone() }),
                    )
                    .await
                {
                    Ok(Ok(resp)) => {
                        if resp.success {
                            submitted_creates.insert(name);
                        }
                    }
                    _ => {
                        // Timeout or RPC error — skip
                    }
                }
            }

            // Drive the node to poll and process pending requests
            let _ = time
                .timeout(
                    Duration::from_secs(5),
                    node.poll_and_process(PollAndProcessRequest),
                )
                .await;

            // Drive the CP to poll responses
            let _ = time
                .timeout(
                    Duration::from_secs(5),
                    cp.poll_responses(PollResponsesRequest),
                )
                .await;
        }

        // Final comparison: ask both services for their VM lists
        let cp_vms = match time
            .timeout(Duration::from_secs(5), cp.list_vms(ListVmsRequest))
            .await
        {
            Ok(Ok(resp)) => resp.vms,
            _ => Vec::new(),
        };

        let node_vms = match time
            .timeout(Duration::from_secs(5), node.list_vms(NodeListVmsRequest))
            .await
        {
            Ok(Ok(resp)) => resp.vms,
            _ => Vec::new(),
        };

        let cp_count = cp_vms.len();
        let node_count = node_vms.len();
        let divergence = node_count.saturating_sub(cp_count);

        // Structural invariant: CP should never have more VMs than Node
        moonpool_sim::assert_always!(
            cp_count <= node_count,
            "cp should never have more VMs than node"
        );

        // Exploration target: find the consistency bug
        if divergence > 0 {
            moonpool_sim::assert_sometimes!(true, "consistency_violation_detected");
        }
        moonpool_sim::assert_sometimes_greater_than!(divergence as i64, 0, "vm_count_divergence");

        Ok(())
    }
}
