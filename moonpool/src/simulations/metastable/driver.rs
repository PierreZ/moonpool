//! ClientDriver workload and shared types.
//!
//! Generates client traffic, measures goodput, and detects metastable failure:
//! DNS is healthy AND goodput < threshold for an extended period.

use std::rc::Rc;
use std::time::Duration;

use crate::{
    JsonCodec, NetTransportBuilder, NetworkAddress, Providers, RpcError, TimeProvider, service,
};
use async_trait::async_trait;
use moonpool_sim::providers::SimProviders;
use moonpool_sim::{SimContext, SimulationResult, Workload};
use serde::{Deserialize, Serialize};

use super::lease_store::{
    BoundLeaseStoreClient, CheckLeaseRequest, CheckLeaseResponse, LeaseStore, LeaseStoreClient,
};

// ============================================================================
// Shared Types
// ============================================================================

/// A lease grant from the LeaseStore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseGrant {
    /// Host that received the lease.
    pub host_id: String,
    /// Time-to-live in ms.
    pub ttl_ms: u64,
}

/// Current status of a host's lease.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseStatus {
    /// Whether the lease is currently valid.
    pub valid: bool,
    /// Remaining TTL in ms.
    pub remaining_ms: u64,
}

/// Status of a host in the fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostStatus {
    /// Host is active and serving traffic.
    Active,
    /// Host is unavailable (lease expired or renewal failed).
    Unavailable,
    /// Host is dead (exceeded max retries).
    Dead,
}

// ============================================================================
// Constants
// ============================================================================

/// Number of hosts in the fleet.
pub const NUM_HOSTS: usize = 16;

/// Goodput measurement interval (sim time).
const MEASUREMENT_INTERVAL_MS: u64 = 500;

/// Number of measurement ticks to run.
const NUM_TICKS: usize = 120;

/// Goodput below this threshold indicates failure state.
const METASTABLE_THRESHOLD: f64 = 0.2;

/// Duration (sim time) of sustained low goodput to declare metastable failure.
const METASTABLE_DURATION_MS: u64 = 5_000;

// ============================================================================
// Fleet Status Service Definition (served by FleetManager, queried by Driver)
// ============================================================================

/// Request to get current fleet status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetStatusRequest;

/// Response with fleet status summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetStatusResponse {
    /// Number of active (serving) hosts.
    pub active: usize,
    /// Number of unavailable hosts.
    pub unavailable: usize,
    /// Number of dead hosts.
    pub dead: usize,
    /// Current concurrent renewal count.
    pub concurrent_renewals: usize,
}

/// FleetStatus RPC interface (served by FleetManager).
///
/// Generates `FleetStatusServiceServer<C>`, `FleetStatusServiceClient`,
/// `BoundFleetStatusServiceClient`.
#[service(id = 0xF1E7_4E00)]
pub trait FleetStatusService {
    /// Get current fleet status.
    async fn get_status(&self, req: FleetStatusRequest) -> Result<FleetStatusResponse, RpcError>;
}

// ============================================================================
// Workload
// ============================================================================

/// Workload that generates client traffic and detects metastable failure.
pub struct DriverWorkload {
    transport: Option<Rc<crate::NetTransport<SimProviders>>>,
    lease_client: Option<BoundLeaseStoreClient<SimProviders, JsonCodec>>,
    fleet_client: Option<BoundFleetStatusServiceClient<SimProviders, JsonCodec>>,
}

impl Default for DriverWorkload {
    fn default() -> Self {
        Self::new()
    }
}

impl DriverWorkload {
    /// Create a new driver workload.
    pub fn new() -> Self {
        Self {
            transport: None,
            lease_client: None,
            fleet_client: None,
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

        // Connect to LeaseStore
        let ls_ip = ctx.peer("lease_store").ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("lease_store not found".into())
        })?;
        let ls_addr = NetworkAddress::parse(&format!("{ls_ip}:4500")).map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("bad ls address: {e}"))
        })?;
        let lease_client = LeaseStoreClient::new(ls_addr).bind(transport.clone(), JsonCodec);

        // Connect to FleetManager
        let fleet_ip = ctx
            .peer("fleet")
            .ok_or_else(|| moonpool_sim::SimulationError::InvalidState("fleet not found".into()))?;
        let fleet_addr = NetworkAddress::parse(&format!("{fleet_ip}:4500")).map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("bad fleet address: {e}"))
        })?;
        let fleet_client =
            FleetStatusServiceClient::new(fleet_addr).bind(transport.clone(), JsonCodec);

        self.transport = Some(transport);
        self.lease_client = Some(lease_client);
        self.fleet_client = Some(fleet_client);

        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let time = ctx.providers().time().clone();
        let lease_client = self.lease_client.as_ref().ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("lease_client not initialized".into())
        })?;
        let fleet_client = self.fleet_client.as_ref().ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("fleet_client not initialized".into())
        })?;

        let mut total_requests: u64 = 0;
        let mut successful_requests: u64 = 0;
        let mut stalled_ticks: i64 = 0;
        let mut peak_concurrent_renewals: i64 = 0;
        let mut metastable_detected = false;
        let mut low_goodput_start_ms: Option<u64> = None;

        // Main measurement loop -- runs for a fixed number of ticks (sim time)
        for _tick in 0..NUM_TICKS {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            let _ = time
                .sleep(Duration::from_millis(MEASUREMENT_INTERVAL_MS))
                .await;

            if ctx.shutdown().is_cancelled() {
                break;
            }

            // Query fleet status
            let fleet_status = match time
                .timeout(
                    Duration::from_secs(2),
                    fleet_client.get_status(FleetStatusRequest),
                )
                .await
            {
                Ok(Ok(resp)) => resp,
                _ => continue,
            };

            let active_hosts = fleet_status.active;
            let concurrent_renewals = fleet_status.concurrent_renewals as i64;

            // Track peak concurrent renewals
            if concurrent_renewals > peak_concurrent_renewals {
                peak_concurrent_renewals = concurrent_renewals;
            }

            // Simulate client requests routed to available hosts
            let batch_size = 8_u64;
            total_requests += batch_size;

            if active_hosts > 0 {
                // Route requests to active hosts -- check their leases
                let mut batch_success = 0_u64;
                for i in 0..batch_size {
                    let host_id = format!("host-{}", i as usize % active_hosts);
                    if let Ok(Ok(CheckLeaseResponse {
                        status: LeaseStatus { valid: true, .. },
                    })) = time
                        .timeout(
                            Duration::from_secs(1),
                            lease_client.check_lease(CheckLeaseRequest { host_id }),
                        )
                        .await
                    {
                        batch_success += 1;
                    }
                }
                successful_requests += batch_success;
            }

            // Calculate goodput
            let goodput = if total_requests > 0 {
                successful_requests as f64 / total_requests as f64
            } else {
                1.0
            };

            // Check DNS health
            let dns_healthy = ctx.state().get::<bool>("dns_healthy").unwrap_or(true);

            // Publish metrics
            ctx.state().publish("goodput", goodput);
            ctx.state().publish("active_hosts", active_hosts);

            // Detect thundering herd
            if concurrent_renewals > (NUM_HOSTS as i64 * 3 / 4) {
                moonpool_sim::assert_sometimes!(true, "thundering_herd_detected");
            }

            // Detect metastable failure: DNS healthy but goodput collapsed
            if dns_healthy && goodput < METASTABLE_THRESHOLD {
                let now_ms = time.now().as_millis() as u64;
                match low_goodput_start_ms {
                    None => {
                        low_goodput_start_ms = Some(now_ms);
                    }
                    Some(start) => {
                        let duration = now_ms.saturating_sub(start);
                        if duration >= METASTABLE_DURATION_MS && !metastable_detected {
                            metastable_detected = true;
                            moonpool_sim::assert_sometimes!(true, "metastable_failure_detected");
                        }
                        if duration >= METASTABLE_DURATION_MS {
                            stalled_ticks += 1;
                        }
                    }
                }
            } else {
                low_goodput_start_ms = None;
            }

            // Publish watermarks for exploration
            moonpool_sim::assert_sometimes_greater_than!(stalled_ticks, 0, "metastable_duration");
            moonpool_sim::assert_sometimes_greater_than!(
                peak_concurrent_renewals,
                0,
                "peak_thundering_herd"
            );
        }

        // Final host count invariant
        let fleet_status = match time
            .timeout(
                Duration::from_secs(2),
                fleet_client.get_status(FleetStatusRequest),
            )
            .await
        {
            Ok(Ok(resp)) => Some(resp),
            _ => None,
        };

        if let Some(status) = fleet_status {
            let total = status.active + status.unavailable + status.dead;
            moonpool_sim::assert_always!(
                total == NUM_HOSTS,
                format!("host count invariant: {} != {}", total, NUM_HOSTS)
            );
        }

        Ok(())
    }
}
