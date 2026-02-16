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
use crate::providers::{SimProviders, SimRandomProvider, SimTimeProvider};
use crate::storage::SimStorageProvider;

use moonpool_core::{Providers, TokioTaskProvider};

use super::topology::WorkloadTopology;

/// Simulation context provided to workloads.
///
/// Wraps all simulation infrastructure into a single, non-generic struct.
/// For code generic over `P: Providers`, pass `ctx.providers()`.
pub struct SimContext {
    providers: SimProviders,
    topology: WorkloadTopology,
    state: StateHandle,
}

impl SimContext {
    /// Create a new simulation context.
    pub fn new(providers: SimProviders, topology: WorkloadTopology, state: StateHandle) -> Self {
        Self {
            providers,
            topology,
            state,
        }
    }

    /// Get the full providers bundle for passing to generic code.
    pub fn providers(&self) -> &SimProviders {
        &self.providers
    }

    /// Get the simulated network provider.
    pub fn network(&self) -> &SimNetworkProvider {
        self.providers.network()
    }

    /// Get the simulated time provider.
    pub fn time(&self) -> &SimTimeProvider {
        self.providers.time()
    }

    /// Get the task provider.
    pub fn task(&self) -> &TokioTaskProvider {
        self.providers.task()
    }

    /// Get the seeded random provider.
    pub fn random(&self) -> &SimRandomProvider {
        self.providers.random()
    }

    /// Get the simulated storage provider.
    pub fn storage(&self) -> &SimStorageProvider {
        self.providers.storage()
    }

    /// Get this workload's IP address.
    pub fn my_ip(&self) -> &str {
        &self.topology.my_ip
    }

    /// Find a peer's IP address by workload name.
    pub fn peer(&self, name: &str) -> Option<String> {
        self.topology.get_peer_by_name(name)
    }

    /// Get all peers as (name, ip) pairs.
    pub fn peers(&self) -> Vec<(String, String)> {
        self.topology
            .peer_names
            .iter()
            .zip(self.topology.peer_ips.iter())
            .map(|(name, ip)| (name.clone(), ip.clone()))
            .collect()
    }

    /// Get the shutdown cancellation token.
    pub fn shutdown(&self) -> &tokio_util::sync::CancellationToken {
        &self.topology.shutdown_signal
    }

    /// Get the shared state handle for cross-workload communication.
    pub fn state(&self) -> &StateHandle {
        &self.state
    }
}
