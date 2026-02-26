//! LeaseStore service and workload (DynamoDB analog).
//!
//! Capacity-limited lease management service. When concurrent requests exceed
//! `MAX_CONCURRENT_LEASES`, latency degrades proportionally and requests
//! may time out. This is the **amplifier** that turns a thundering herd into
//! sustained overload.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use crate::{
    JsonCodec, NetTransportBuilder, NetworkAddress, Providers, RpcError, ServerHandle,
    TimeProvider, service,
};
use async_trait::async_trait;
use moonpool_sim::providers::SimProviders;
use moonpool_sim::{SimContext, SimulationResult, Workload};
use serde::{Deserialize, Serialize};

use super::driver::{LeaseGrant, LeaseStatus};

// ============================================================================
// Message Types
// ============================================================================

/// Request to renew a host's lease.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewLeaseRequest {
    /// Host identifier.
    pub host_id: String,
}

/// Response from a lease renewal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewLeaseResponse {
    /// The lease grant, or None on capacity failure.
    pub grant: Option<LeaseGrant>,
}

/// Request to check a host's lease status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckLeaseRequest {
    /// Host identifier.
    pub host_id: String,
}

/// Response from a lease check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckLeaseResponse {
    /// Current lease status.
    pub status: LeaseStatus,
}

// ============================================================================
// Service Definition
// ============================================================================

/// LeaseStore RPC interface.
///
/// Generates `LeaseStoreServer<C>`, `LeaseStoreClient`, `BoundLeaseStoreClient`.
#[service(id = 0x1EA5_4E00)]
pub trait LeaseStore {
    /// Renew a host's lease.
    async fn renew_lease(&self, req: RenewLeaseRequest) -> Result<RenewLeaseResponse, RpcError>;

    /// Check a host's lease status.
    async fn check_lease(&self, req: CheckLeaseRequest) -> Result<CheckLeaseResponse, RpcError>;
}

// ============================================================================
// Constants
// ============================================================================

/// Maximum concurrent lease operations before degradation.
const MAX_CONCURRENT_LEASES: usize = 4;

/// Base latency for a lease operation (sim time).
const LEASE_OP_BASE_MS: u64 = 100;

/// Lease TTL (sim time).
pub const LEASE_TTL_MS: u64 = 10_000;

// ============================================================================
// Handler
// ============================================================================

/// Internal lease entry.
struct LeaseEntry {
    /// When the lease was last renewed (sim time ms).
    renewed_at_ms: u64,
    /// Lease TTL in ms.
    ttl_ms: u64,
}

/// LeaseStore handler with capacity tracking.
pub struct LeaseStoreHandler<T: TimeProvider> {
    /// Active leases: host_id -> entry.
    leases: RefCell<HashMap<String, LeaseEntry>>,
    /// Current number of in-flight requests.
    in_flight: Cell<usize>,
    /// Time provider for latency simulation and current time.
    time: T,
}

impl<T: TimeProvider> LeaseStoreHandler<T> {
    /// Create a new handler.
    pub fn new(time: T) -> Self {
        Self {
            leases: RefCell::new(HashMap::new()),
            in_flight: Cell::new(0),
            time,
        }
    }
}

#[async_trait(?Send)]
impl<T: TimeProvider + 'static> LeaseStore for LeaseStoreHandler<T> {
    async fn renew_lease(&self, req: RenewLeaseRequest) -> Result<RenewLeaseResponse, RpcError> {
        let current = self.in_flight.get();
        self.in_flight.set(current + 1);

        // Simulate processing latency proportional to load
        let load_factor = if current >= MAX_CONCURRENT_LEASES {
            moonpool_sim::assert_sometimes!(true, "lease_store_overloaded");
            // Under overload: latency scales with queue depth
            (current as u64) * LEASE_OP_BASE_MS
        } else {
            LEASE_OP_BASE_MS
        };
        let _ = self.time.sleep(Duration::from_millis(load_factor)).await;

        let result = if current >= MAX_CONCURRENT_LEASES * 2 {
            // Severely overloaded: reject
            Ok(RenewLeaseResponse { grant: None })
        } else {
            let now = self.time.now().as_millis() as u64;
            self.leases.borrow_mut().insert(
                req.host_id.clone(),
                LeaseEntry {
                    renewed_at_ms: now,
                    ttl_ms: LEASE_TTL_MS,
                },
            );
            Ok(RenewLeaseResponse {
                grant: Some(LeaseGrant {
                    host_id: req.host_id,
                    ttl_ms: LEASE_TTL_MS,
                }),
            })
        };

        self.in_flight.set(self.in_flight.get().saturating_sub(1));
        result
    }

    async fn check_lease(&self, req: CheckLeaseRequest) -> Result<CheckLeaseResponse, RpcError> {
        let now = self.time.now().as_millis() as u64;
        let leases = self.leases.borrow();
        let status = if let Some(entry) = leases.get(&req.host_id) {
            let elapsed = now.saturating_sub(entry.renewed_at_ms);
            if elapsed < entry.ttl_ms {
                LeaseStatus {
                    valid: true,
                    remaining_ms: entry.ttl_ms - elapsed,
                }
            } else {
                LeaseStatus {
                    valid: false,
                    remaining_ms: 0,
                }
            }
        } else {
            LeaseStatus {
                valid: false,
                remaining_ms: 0,
            }
        };
        Ok(CheckLeaseResponse { status })
    }
}

// ============================================================================
// Workload
// ============================================================================

/// Workload that hosts the LeaseStore RPC server.
pub struct LeaseStoreWorkload {
    transport: Option<Rc<crate::NetTransport<SimProviders>>>,
    server_handle: Option<ServerHandle>,
}

impl Default for LeaseStoreWorkload {
    fn default() -> Self {
        Self::new()
    }
}

impl LeaseStoreWorkload {
    /// Create a new LeaseStore workload.
    pub fn new() -> Self {
        Self {
            transport: None,
            server_handle: None,
        }
    }
}

#[async_trait(?Send)]
impl Workload for LeaseStoreWorkload {
    fn name(&self) -> &str {
        "lease_store"
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

        let time = ctx.providers().time().clone();
        let handler = Rc::new(LeaseStoreHandler::new(time));

        let server = LeaseStoreServer::init(&transport, JsonCodec);
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
