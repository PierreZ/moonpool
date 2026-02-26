//! FleetManager workload (DWFM analog).
//!
//! Manages NUM_HOSTS hosts, each with an autonomous lease renewal loop.
//! When DNS becomes unreachable, all leases expire. When DNS recovers,
//! all hosts try to renew simultaneously -- the **thundering herd** that
//! sustains the metastable failure.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::{
    JsonCodec, NetTransportBuilder, NetworkAddress, Providers, RpcError, ServerHandle,
    TaskProvider, TimeProvider,
};
use async_trait::async_trait;
use moonpool_sim::providers::SimProviders;
use moonpool_sim::{SimContext, SimulationResult, Workload};

use super::dns::{BoundDnsManagerClient, DnsManager, DnsManagerClient, ResolveRequest};
use super::driver::{
    FleetStatusRequest, FleetStatusResponse, FleetStatusService, HostStatus, NUM_HOSTS,
};
use super::lease_store::{
    BoundLeaseStoreClient, LEASE_TTL_MS, LeaseStore, LeaseStoreClient, RenewLeaseRequest,
};

// ============================================================================
// Constants
// ============================================================================

/// How often hosts renew their leases (TTL / 2).
const RENEWAL_INTERVAL_MS: u64 = LEASE_TTL_MS / 2;

/// Client-side timeout for lease operations.
const LEASE_OP_TIMEOUT_MS: u64 = 2_000;

/// Maximum retries before a host becomes dead.
const MAX_RETRIES: usize = 4;

/// Initial retry backoff.
const RETRY_BASE_MS: u64 = 500;

/// Maximum retry backoff.
const RETRY_MAX_MS: u64 = 4_000;

// ============================================================================
// Fleet State
// ============================================================================

/// Shared fleet state accessible by all host tasks and the status service.
pub struct FleetState {
    /// Per-host status.
    pub hosts: RefCell<Vec<HostStatus>>,
    /// Current number of concurrent renewal attempts.
    pub concurrent_renewals: Cell<usize>,
}

impl FleetState {
    /// Create fleet state for n hosts, all starting as Active.
    pub fn new(n: usize) -> Self {
        Self {
            hosts: RefCell::new(vec![HostStatus::Active; n]),
            concurrent_renewals: Cell::new(0),
        }
    }

    /// Count hosts by status.
    pub fn counts(&self) -> (usize, usize, usize) {
        let hosts = self.hosts.borrow();
        let active = hosts.iter().filter(|h| **h == HostStatus::Active).count();
        let unavailable = hosts
            .iter()
            .filter(|h| **h == HostStatus::Unavailable)
            .count();
        let dead = hosts.iter().filter(|h| **h == HostStatus::Dead).count();
        (active, unavailable, dead)
    }
}

// ============================================================================
// FleetStatus Handler
// ============================================================================

/// Handler for the FleetStatus RPC (allows driver to query fleet state).
pub struct FleetStatusHandler {
    state: Rc<FleetState>,
}

impl FleetStatusHandler {
    /// Create handler with shared fleet state.
    pub fn new(state: Rc<FleetState>) -> Self {
        Self { state }
    }
}

#[async_trait(?Send)]
impl FleetStatusService for FleetStatusHandler {
    async fn get_status(&self, _req: FleetStatusRequest) -> Result<FleetStatusResponse, RpcError> {
        let (active, unavailable, dead) = self.state.counts();
        Ok(FleetStatusResponse {
            active,
            unavailable,
            dead,
            concurrent_renewals: self.state.concurrent_renewals.get(),
        })
    }
}

// ============================================================================
// Workload
// ============================================================================

/// Workload that manages a fleet of hosts with autonomous lease renewal.
pub struct FleetWorkload {
    transport: Option<Rc<crate::NetTransport<SimProviders>>>,
    server_handle: Option<ServerHandle>,
    fleet_state: Option<Rc<FleetState>>,
}

impl Default for FleetWorkload {
    fn default() -> Self {
        Self::new()
    }
}

impl FleetWorkload {
    /// Create a new fleet workload.
    pub fn new() -> Self {
        Self {
            transport: None,
            server_handle: None,
            fleet_state: None,
        }
    }
}

#[async_trait(?Send)]
impl Workload for FleetWorkload {
    fn name(&self) -> &str {
        "fleet"
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

        let fleet_state = Rc::new(FleetState::new(NUM_HOSTS));

        // Serve FleetStatus RPC
        let handler = Rc::new(FleetStatusHandler::new(fleet_state.clone()));
        let server = super::driver::FleetStatusServiceServer::init(&transport, JsonCodec);
        let handle = server.serve(transport.clone(), handler, ctx.providers());

        self.fleet_state = Some(fleet_state);
        self.transport = Some(transport);
        self.server_handle = Some(handle);

        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let time = ctx.providers().time().clone();
        let task = ctx.providers().task().clone();
        let transport = self.transport.as_ref().ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("transport not initialized".into())
        })?;

        let fleet_state = self
            .fleet_state
            .as_ref()
            .ok_or_else(|| {
                moonpool_sim::SimulationError::InvalidState("fleet_state missing".into())
            })?
            .clone();

        // Connect to DNS
        let dns_ip = ctx
            .peer("dns")
            .ok_or_else(|| moonpool_sim::SimulationError::InvalidState("dns not found".into()))?;
        let dns_addr = NetworkAddress::parse(&format!("{dns_ip}:4500")).map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("bad dns address: {e}"))
        })?;
        let dns_client = DnsManagerClient::new(dns_addr).bind(transport.clone(), JsonCodec);
        let dns_client = Rc::new(dns_client);

        // Connect to LeaseStore
        let ls_ip = ctx.peer("lease_store").ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("lease_store not found".into())
        })?;
        let ls_addr = NetworkAddress::parse(&format!("{ls_ip}:4500")).map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("bad ls address: {e}"))
        })?;
        let ls_client = LeaseStoreClient::new(ls_addr).bind(transport.clone(), JsonCodec);
        let ls_client = Rc::new(ls_client);

        // Shared shutdown flag for spawned tasks
        let should_stop = Rc::new(Cell::new(false));

        // Spawn a renewal task for each host
        for host_idx in 0..NUM_HOSTS {
            let host_id = format!("host-{host_idx}");
            let time = time.clone();
            let dns = dns_client.clone();
            let ls = ls_client.clone();
            let state = fleet_state.clone();
            let stop = should_stop.clone();

            task.spawn_task(&format!("host-{host_idx}"), async move {
                host_renewal_loop(host_idx, host_id, &time, dns, ls, state, stop).await;
            });
        }

        // Wait for shutdown
        ctx.shutdown().cancelled().await;
        should_stop.set(true);

        // Give spawned tasks a chance to see the flag
        let _ = time.sleep(Duration::from_millis(1)).await;

        self.server_handle.take();
        Ok(())
    }
}

/// Autonomous renewal loop for a single host.
async fn host_renewal_loop<T: TimeProvider>(
    host_idx: usize,
    host_id: String,
    time: &T,
    dns: Rc<BoundDnsManagerClient<SimProviders, JsonCodec>>,
    ls: Rc<BoundLeaseStoreClient<SimProviders, JsonCodec>>,
    state: Rc<FleetState>,
    should_stop: Rc<Cell<bool>>,
) {
    let mut retry_count: usize = 0;
    let mut backoff_ms: u64 = RETRY_BASE_MS;

    // Initial lease acquisition
    let _ = attempt_renewal(&host_id, time, &dns, &ls, &state).await;

    loop {
        if should_stop.get() {
            break;
        }

        // Sleep until next renewal
        let sleep_ms = if retry_count > 0 {
            // In retry mode: use backoff with jitter
            let jitter = moonpool_sim::sim_random_range(0..(backoff_ms / 4 + 1));
            backoff_ms + jitter
        } else {
            // Normal mode: periodic renewal
            RENEWAL_INTERVAL_MS
        };

        let _ = time.sleep(Duration::from_millis(sleep_ms)).await;

        if should_stop.get() {
            break;
        }

        match attempt_renewal(&host_id, time, &dns, &ls, &state).await {
            RenewalResult::Success => {
                state.hosts.borrow_mut()[host_idx] = HostStatus::Active;
                retry_count = 0;
                backoff_ms = RETRY_BASE_MS;
            }
            RenewalResult::DnsFailure | RenewalResult::LeaseFailure => {
                retry_count += 1;
                if retry_count > MAX_RETRIES {
                    state.hosts.borrow_mut()[host_idx] = HostStatus::Dead;
                    // Dead hosts wait then attempt recovery
                    let _ = time.sleep(Duration::from_millis(LEASE_TTL_MS * 2)).await;
                    if should_stop.get() {
                        break;
                    }
                    retry_count = 0;
                    backoff_ms = RETRY_BASE_MS;
                    state.hosts.borrow_mut()[host_idx] = HostStatus::Unavailable;
                } else {
                    state.hosts.borrow_mut()[host_idx] = HostStatus::Unavailable;
                    backoff_ms = (backoff_ms * 2).min(RETRY_MAX_MS);
                }
            }
        }
    }
}

/// Result of a single renewal attempt.
enum RenewalResult {
    Success,
    DnsFailure,
    LeaseFailure,
}

/// Attempt a single lease renewal: resolve DNS then call LeaseStore.
async fn attempt_renewal<T: TimeProvider>(
    host_id: &str,
    time: &T,
    dns: &BoundDnsManagerClient<SimProviders, JsonCodec>,
    ls: &BoundLeaseStoreClient<SimProviders, JsonCodec>,
    state: &FleetState,
) -> RenewalResult {
    // Step 1: Resolve DNS for lease_store
    let dns_result = time
        .timeout(
            Duration::from_secs(1),
            dns.resolve(ResolveRequest {
                name: "lease_store".to_string(),
            }),
        )
        .await;

    match dns_result {
        Ok(Ok(resp)) => {
            if resp.ip.is_none() {
                return RenewalResult::DnsFailure;
            }
        }
        _ => return RenewalResult::DnsFailure,
    }

    // Step 2: Renew lease
    state
        .concurrent_renewals
        .set(state.concurrent_renewals.get() + 1);

    let result = time
        .timeout(
            Duration::from_millis(LEASE_OP_TIMEOUT_MS),
            ls.renew_lease(RenewLeaseRequest {
                host_id: host_id.to_string(),
            }),
        )
        .await;

    state
        .concurrent_renewals
        .set(state.concurrent_renewals.get().saturating_sub(1));

    match result {
        Ok(Ok(resp)) => {
            if resp.grant.is_some() {
                RenewalResult::Success
            } else {
                RenewalResult::LeaseFailure
            }
        }
        _ => RenewalResult::LeaseFailure,
    }
}
