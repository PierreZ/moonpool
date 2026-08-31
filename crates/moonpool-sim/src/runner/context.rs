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

use std::sync::Arc;

use moonpool_core::metrics::MetricsSource;

use crate::chaos::state_handle::StateHandle;
use crate::network::SimNetworkProvider;
use crate::observability::SimulationLayerHandle;
use crate::providers::{SimProviders, SimRandomProvider, SimTimeProvider};
use crate::storage::SimStorageProvider;

use moonpool_core::Providers;

use super::app_metrics::MetricsHandle;
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
    metrics: MetricsHandle,
}

impl SimContext {
    /// Create a new simulation context.
    #[must_use]
    pub fn new(
        providers: SimProviders,
        topology: WorkloadTopology,
        state: StateHandle,
        obs: SimulationLayerHandle,
        metrics: MetricsHandle,
    ) -> Self {
        Self {
            providers,
            topology,
            state,
            obs,
            metrics,
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

    /// Get a simulated block-device provider scoped to this workload's
    /// process IP. Process crashes resolve its buffered writes through the
    /// barrier-bounded crash model; `CrashAndWipe` reboots erase its devices.
    ///
    /// # Panics
    ///
    /// Panics if the simulation has already shut down.
    #[must_use]
    pub fn block_devices(&self) -> crate::storage::SimBlockDeviceProvider {
        self.providers.block_devices()
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

    /// Get this node's application metrics source, downcast to its type.
    ///
    /// The source is the one
    /// [`metrics_factory`](super::builder::SimulationBuilder::metrics_factory)
    /// built for [`my_ip`](Self::my_ip), so each simulated node counts
    /// independently. Returns `None` when no factory was configured, or when
    /// `S` is not the type the factory produced.
    ///
    /// ```ignore
    /// let metrics = ctx.metrics::<PrometheusSource>().expect("metrics configured");
    /// metrics.int_counter("requests_total", "Requests served")?.inc();
    /// ```
    ///
    /// The source outlives process reboots (it is keyed by IP, not by process
    /// instance), so a metric registered on first boot is still registered
    /// after a restart — look counters up rather than re-registering them.
    #[must_use]
    pub fn metrics<S: MetricsSource>(&self) -> Option<Arc<S>> {
        self.metrics.get(self.my_ip())
    }

    /// Get the raw handle to every node's metrics sources.
    ///
    /// Workloads use this to read a *server's* counters in `check()`;
    /// [`metrics`](Self::metrics) is the usual per-node accessor.
    #[must_use]
    pub fn metrics_handle(&self) -> &MetricsHandle {
        &self.metrics
    }
}
