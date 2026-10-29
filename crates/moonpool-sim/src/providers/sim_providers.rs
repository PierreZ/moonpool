//! Simulation providers bundle implementation.

use std::net::IpAddr;

use moonpool_core::impl_providers_bundle;

use crate::network::SimNetworkProvider;
use crate::sim::WeakSimWorld;
use crate::storage::SimStorageProvider;

use super::{SimRandomProvider, SimSystemProvider, SimTaskProvider, SimTimeProvider};

/// Simulation providers bundle for deterministic testing.
///
/// This struct bundles all simulation-based providers into a single
/// instance that implements [`Providers`](moonpool_core::Providers). Each bundle is scoped to a
/// specific process IP for per-process storage fault injection.
///
/// ## Usage
///
/// The runner builds one bundle per process and hands it over through
/// [`SimContext::providers`](crate::SimContext::providers). A test that
/// drives a [`SimWorld`](crate::SimWorld) by hand builds its own:
///
/// ```rust
/// use moonpool_sim::{Providers, SimProviders, SimWorld};
///
/// let sim = SimWorld::new();
/// let ip = std::net::IpAddr::from([10, 0, 1, 1]);
/// let providers = SimProviders::new(sim.downgrade(), ip);
///
/// // Access individual providers.
/// let _network = providers.network();
/// let _storage = providers.storage();
/// ```
///
/// ## Implementation Notes
///
/// - Uses `SimNetworkProvider` for simulated TCP connections
/// - Uses `SimTimeProvider` for logical/simulated time
/// - Uses `SimTaskProvider` for task spawning on the deterministic executor
/// - Uses `SimRandomProvider` for the simulation's seeded thread-local
///   randomness (seeded once per run by the runner or by `SimWorld`
///   construction; building a bundle never reseeds it)
/// - Uses `SimStorageProvider` for simulated file I/O with per-process fault injection
/// - Uses `SimSystemProvider` for the per-process simulated disk counters
#[derive(Clone)]
pub struct SimProviders {
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task: SimTaskProvider,
    random: SimRandomProvider,
    storage: SimStorageProvider,
    system: SimSystemProvider,
}

impl SimProviders {
    /// Create a new simulation providers bundle scoped to a process IP.
    ///
    /// # Arguments
    ///
    /// * `sim` - Weak reference to the simulation world
    /// * `ip` - IP address of the owning process (for per-process storage scoping)
    #[must_use]
    pub fn new(sim: WeakSimWorld, ip: IpAddr) -> Self {
        Self {
            network: SimNetworkProvider::new(sim.clone(), ip),
            time: SimTimeProvider::new(sim.clone()),
            task: SimTaskProvider::default(),
            random: SimRandomProvider::new(),
            storage: SimStorageProvider::new(sim.clone(), ip),
            system: SimSystemProvider::new(sim, ip),
        }
    }

    /// Spawn through `task` instead of an unscoped task provider.
    ///
    /// The runner passes the actor's
    /// [`ProviderEnv::task`](crate::ProviderEnv::task): bound to the boot
    /// scope a process kill cancels, and recording the panics of the tasks
    /// it spawns.
    #[must_use]
    pub fn with_task(mut self, task: SimTaskProvider) -> Self {
        self.task = task;
        self
    }
}

impl_providers_bundle!(SimProviders {
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task: SimTaskProvider,
    random: SimRandomProvider,
    storage: SimStorageProvider,
    system: SimSystemProvider,
});
