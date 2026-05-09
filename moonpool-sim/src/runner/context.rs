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

use std::any::Any;
use std::fmt::Debug;

use crate::chaos::state_handle::StateHandle;
use crate::network::SimNetworkProvider;
use crate::observability::{SimulationLayerHandle, TypedEntry};
use crate::providers::{SimProviders, SimRandomProvider, SimTimeProvider};
use crate::storage::SimStorageProvider;

use moonpool_core::{Providers, TimeProvider, TokioTaskProvider};

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

    /// Get this workload's client ID.
    ///
    /// Assigned by the builder's [`ClientId`](crate::ClientId) strategy.
    /// Defaults to sequential IDs starting from 0 (FDB-style).
    pub fn client_id(&self) -> usize {
        self.topology.client_id
    }

    /// Get the total number of workload instances sharing this entry.
    ///
    /// For single `.workload()` entries this is 1.
    /// For `.workloads(count, factory)` entries this is the resolved count.
    pub fn client_count(&self) -> usize {
        self.topology.client_count
    }

    /// Find a peer's IP address by workload name.
    pub fn peer(&self, name: &str) -> Option<String> {
        self.topology.peer_by_name(name)
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

    /// Get the workload topology (peer IPs, process IPs, tags, etc.).
    pub fn topology(&self) -> &WorkloadTopology {
        &self.topology
    }

    /// Get the shared state handle for cross-workload communication.
    pub fn state(&self) -> &StateHandle {
        &self.state
    }

    /// Emit a typed event to a named timeline.
    ///
    /// Routes through `tracing::event!` at target `"moonpool::sim"`. With a
    /// [`crate::observability::SimulationLayer`] installed (the standard sim
    /// configuration), the layer captures the typed payload and runs invariants.
    /// Without a layer (e.g. in production), the event still reaches subscribers
    /// like `fmt` or OTel with its `Debug` payload.
    ///
    /// Captures simulation time and this process's IP address automatically.
    pub fn emit<T>(&self, key: &'static str, event: T)
    where
        T: Any + Debug + Send + Sync + 'static,
    {
        let time_ms = self.time().now().as_millis() as u64;
        let source = self.my_ip().to_string();
        crate::sim_emit!(key, time_ms, source, event);
    }

    /// Read all captured events under `key` as typed entries.
    ///
    /// Returns an empty vector if no [`crate::observability::SimulationLayer`]
    /// is installed, no events were captured, or none of them match `T`.
    pub fn timeline<T: Any + Clone + 'static>(&self, key: &'static str) -> Vec<TypedEntry<T>> {
        self.obs.timeline(key)
    }

    /// Get a clonable handle to the observability layer, for advanced
    /// post-simulation inspection.
    pub fn observability(&self) -> &SimulationLayerHandle {
        &self.obs
    }
}
