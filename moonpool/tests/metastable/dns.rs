//! DnsManager service and workload.
//!
//! Provides service discovery for the fleet. Contains a latent race condition:
//! with P≈0.10 via `sim_random()`, a concurrent "enactor" deletes DNS records,
//! making the LeaseStore unreachable. The DNS self-heals after 2-5s sim time.
//! This is the **trigger** for the metastable failure.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use moonpool::{
    JsonCodec, NetTransportBuilder, NetworkAddress, Providers, RpcError, ServerHandle,
    TimeProvider, service,
};
use moonpool_sim::providers::SimProviders;
use moonpool_sim::{SimContext, SimulationResult, Workload};
use serde::{Deserialize, Serialize};

// ============================================================================
// Message Types
// ============================================================================

/// Request to resolve a service name to an IP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveRequest {
    /// Service name to look up.
    pub name: String,
}

/// Response from a DNS resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveResponse {
    /// Resolved IP address, or None if not found.
    pub ip: Option<String>,
}

// ============================================================================
// Service Definition
// ============================================================================

/// DnsManager RPC interface.
///
/// Generates `DnsManagerServer<C>`, `DnsManagerClient`, `BoundDnsManagerClient`.
#[service(id = 0xD415_4E00)]
pub trait DnsManager {
    /// Resolve a service name to an IP address.
    async fn resolve(&self, req: ResolveRequest) -> Result<ResolveResponse, RpcError>;
}

// ============================================================================
// Handler
// ============================================================================

/// DnsManager handler with records map and race condition state.
pub struct DnsManagerHandler {
    /// Service name → IP address mappings.
    records: Rc<RefCell<HashMap<String, String>>>,
    /// Whether the DNS race condition is currently active.
    race_active: Rc<RefCell<bool>>,
}

impl DnsManagerHandler {
    /// Create a new handler with the given records.
    pub fn new(records: HashMap<String, String>) -> Self {
        Self {
            records: Rc::new(RefCell::new(records)),
            race_active: Rc::new(RefCell::new(false)),
        }
    }

    /// Get a reference to the race_active flag (for the enactor loop).
    pub fn race_active(&self) -> Rc<RefCell<bool>> {
        self.race_active.clone()
    }

    /// Get a reference to the records (for the enactor loop).
    pub fn records(&self) -> Rc<RefCell<HashMap<String, String>>> {
        self.records.clone()
    }
}

#[async_trait(?Send)]
impl DnsManager for DnsManagerHandler {
    async fn resolve(&self, req: ResolveRequest) -> Result<ResolveResponse, RpcError> {
        let ip = if *self.race_active.borrow() {
            // During race condition, DNS records are missing
            None
        } else {
            self.records.borrow().get(&req.name).cloned()
        };
        Ok(ResolveResponse { ip })
    }
}

// ============================================================================
// Constants
// ============================================================================

/// Probability of the DNS race condition triggering per check cycle.
const DNS_RACE_PROB: f64 = 0.10;

/// Minimum duration for DNS self-heal (sim time).
const DNS_HEAL_MIN_MS: u64 = 2_000;

/// Maximum duration for DNS self-heal (sim time).
const DNS_HEAL_MAX_MS: u64 = 5_000;

/// How often the enactor checks for race condition (sim time).
const DNS_CHECK_INTERVAL_MS: u64 = 1_000;

// ============================================================================
// Workload
// ============================================================================

/// Workload that hosts the DnsManager RPC server and runs the race condition
/// enactor loop.
pub struct DnsWorkload {
    transport: Option<Rc<moonpool::NetTransport<SimProviders>>>,
    server_handle: Option<ServerHandle>,
    /// Shared race condition flag (set in setup, used in run).
    race_active: Option<Rc<RefCell<bool>>>,
    /// Shared records (set in setup, used in run).
    records: Option<Rc<RefCell<HashMap<String, String>>>>,
    /// Backup of initial records for healing.
    initial_records: HashMap<String, String>,
}

impl DnsWorkload {
    /// Create a new DNS workload.
    pub fn new() -> Self {
        Self {
            transport: None,
            server_handle: None,
            race_active: None,
            records: None,
            initial_records: HashMap::new(),
        }
    }
}

#[async_trait(?Send)]
impl Workload for DnsWorkload {
    fn name(&self) -> &str {
        "dns"
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

        // Populate DNS records from simulation peer IPs
        let mut records = HashMap::new();
        if let Some(ls_ip) = ctx.peer("lease_store") {
            records.insert("lease_store".to_string(), ls_ip.to_string());
        }
        self.initial_records = records.clone();

        let handler = Rc::new(DnsManagerHandler::new(records));
        self.race_active = Some(handler.race_active());
        self.records = Some(handler.records());

        let server = DnsManagerServer::init(&transport, JsonCodec);
        let handle = server.serve(transport.clone(), handler, ctx.providers());

        self.transport = Some(transport);
        self.server_handle = Some(handle);

        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let time = ctx.providers().time().clone();
        let race_active = self
            .race_active
            .as_ref()
            .ok_or_else(|| {
                moonpool_sim::SimulationError::InvalidState("race_active not initialized".into())
            })?
            .clone();
        let records = self
            .records
            .as_ref()
            .ok_or_else(|| {
                moonpool_sim::SimulationError::InvalidState("records not initialized".into())
            })?
            .clone();

        let backup_records = self.initial_records.clone();

        // Enactor loop: periodically checks for race condition
        loop {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            let _ = time
                .sleep(Duration::from_millis(DNS_CHECK_INTERVAL_MS))
                .await;

            if ctx.shutdown().is_cancelled() {
                break;
            }

            let already_active = *race_active.borrow();

            if !already_active && moonpool_sim::sim_random::<f64>() < DNS_RACE_PROB {
                // Trigger DNS race condition
                moonpool_sim::assert_sometimes!(true, "dns_race_triggered");
                *race_active.borrow_mut() = true;
                ctx.state().publish("dns_healthy", false);

                // Self-heal after random duration
                let heal_ms = moonpool_sim::sim_random_range(DNS_HEAL_MIN_MS..DNS_HEAL_MAX_MS);
                let _ = time.sleep(Duration::from_millis(heal_ms)).await;

                // Restore records and mark healthy
                *records.borrow_mut() = backup_records.clone();
                *race_active.borrow_mut() = false;
                ctx.state().publish("dns_healthy", true);
            } else if !already_active {
                ctx.state().publish("dns_healthy", true);
            }
        }

        self.server_handle.take();
        Ok(())
    }
}
