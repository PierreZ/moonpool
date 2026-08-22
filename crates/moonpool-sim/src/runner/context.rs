//! Simulation context for workloads.
//!
//! [`SimContext`] is the single entry point for workloads to access simulation
//! infrastructure: providers, topology, shared state, and shutdown signaling.
//!
//! # Usage
//!
//! ```ignore
//! use moonpool_sim::SimContext;
//!
//! async fn my_workload(ctx: &SimContext) -> SimulationResult<()> {
//!     let server_ip = ctx.peer("server").expect("server not found");
//!     let stream = ctx.network().connect(&server_ip).await?;
//!     ctx.state().publish("connected", true);
//!     Ok(())
//! }
//! ```

use crate::chaos::state_handle::StateHandle;
use crate::network::SimNetworkProvider;
use crate::observability::SimulationLayerHandle;
use crate::providers::{SimProviders, SimRandomProvider, SimTimeProvider};
use crate::storage::SimStorageProvider;

use moonpool_core::Providers;

use super::topology::WorkloadTopology;

/// Simulation context provided to workloads.
///
/// Wraps all simulation infrastructure into a single, non-generic struct.
/// For code generic over `P: Providers`, pass `ctx.providers()`.
pub struct SimContext {
    providers: SimProviders,
    topology: WorkloadTopology,
    state: StateHandle,
    obs: SimulationLayerHandle,
}

impl SimContext {
    /// Create a new simulation context.
    #[must_use]
    pub fn new(
        providers: SimProviders,
        topology: WorkloadTopology,
        state: StateHandle,
        obs: SimulationLayerHandle,
    ) -> Self {
        Self {
            providers,
            topology,
            state,
            obs,
        }
    }

    /// Get the full providers bundle for passing to generic code.
    #[must_use]
    pub fn providers(&self) -> &SimProviders {
        &self.providers
    }

    /// Get the simulated network provider.
    #[must_use]
    pub fn network(&self) -> &SimNetworkProvider {
        self.providers.network()
    }

    /// Get the simulated time provider.
    #[must_use]
    pub fn time(&self) -> &SimTimeProvider {
        self.providers.time()
    }

    /// Get the task provider.
    #[must_use]
    pub fn task(&self) -> &crate::providers::SimTaskProvider {
        self.providers.task()
    }

    /// Get the seeded random provider.
    #[must_use]
    pub fn random(&self) -> &SimRandomProvider {
        self.providers.random()
    }

    /// Get the simulated storage provider.
    #[must_use]
    pub fn storage(&self) -> &SimStorageProvider {
        self.providers.storage()
    }

    /// Get this workload's IP address.
    #[must_use]
    pub fn my_ip(&self) -> &str {
        &self.topology.my_ip
    }

    /// Get this workload's client ID.
    ///
    /// Assigned by the builder's [`ClientId`](crate::ClientId) strategy.
    /// Defaults to sequential IDs starting from 0 (FDB-style).
    #[must_use]
    pub fn client_id(&self) -> usize {
        self.topology.client_id
    }

    /// Get the total number of workload instances sharing this entry.
    ///
    /// For single `.workload()` entries this is 1.
    /// For `.workloads(count, factory)` entries this is the resolved count.
    #[must_use]
    pub fn client_count(&self) -> usize {
        self.topology.client_count
    }

    /// Find a peer's IP address by workload name.
    #[must_use]
    pub fn peer(&self, name: &str) -> Option<String> {
        self.topology.peer_by_name(name)
    }

    /// Get all peers as (name, ip) pairs.
    #[must_use]
    pub fn peers(&self) -> Vec<(String, String)> {
        self.topology
            .peer_names
            .iter()
            .zip(self.topology.peer_ips.iter())
            .map(|(name, ip)| (name.clone(), ip.clone()))
            .collect()
    }

    /// Get the shutdown cancellation token.
    #[must_use]
    pub fn shutdown(&self) -> &tokio_util::sync::CancellationToken {
        &self.topology.shutdown_signal
    }

    /// Get the workload topology (peer IPs, process IPs, tags, etc.).
    #[must_use]
    pub fn topology(&self) -> &WorkloadTopology {
        &self.topology
    }

    /// Get the shared state handle for cross-workload communication.
    #[must_use]
    pub fn state(&self) -> &StateHandle {
        &self.state
    }

    /// Get a clonable handle to the observability layer.
    ///
    /// The handle implements [`crate::TraceQuery`], so workloads can read
    /// the captured timeline (e.g. in `check()`) the same way invariants do.
    /// To get events INTO the timeline, emit plain `tracing` events — e.g.
    /// `tracing::info!(term, leader = %ip, "leader_elected")` — from inside
    /// a process or workload task; the orchestrator's actor spans attribute
    /// them automatically.
    #[must_use]
    pub fn observability(&self) -> &SimulationLayerHandle {
        &self.obs
    }
}
