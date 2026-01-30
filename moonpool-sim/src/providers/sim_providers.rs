//! Simulation providers bundle implementation.

use moonpool_core::{Providers, TokioStorageProvider, TokioTaskProvider};

use crate::network::SimNetworkProvider;
use crate::sim::WeakSimWorld;

use super::{SimRandomProvider, SimTimeProvider};

/// Simulation providers bundle for deterministic testing.
///
/// This struct bundles all four simulation-based providers into a single
/// instance that implements [`Providers`].
///
/// ## Usage
///
/// `SimProviders` is typically created by the simulation framework during
/// workload setup, but can also be manually constructed:
///
/// ```rust,ignore
/// use moonpool_sim::{SimWorld, SimProviders, Providers};
///
/// let sim = SimWorld::new();
/// let providers = SimProviders::new(sim.downgrade(), 42);
///
/// // Access individual providers
/// let network = providers.network();
/// let time = providers.time();
/// ```
///
/// ## Implementation Notes
///
/// - Uses `SimNetworkProvider` for simulated TCP connections
/// - Uses `SimTimeProvider` for logical/simulated time
/// - Uses `TokioTaskProvider` for task spawning (same as production)
/// - Uses `SimRandomProvider` for seeded deterministic randomness
#[derive(Clone)]
pub struct SimProviders {
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task: TokioTaskProvider,
    random: SimRandomProvider,
    // TODO: Replace with SimStorageProvider when implemented
    storage: TokioStorageProvider,
}

impl SimProviders {
    /// Create a new simulation providers bundle.
    ///
    /// # Arguments
    ///
    /// * `sim` - Weak reference to the simulation world
    /// * `seed` - Seed for deterministic random number generation
    pub fn new(sim: WeakSimWorld, seed: u64) -> Self {
        Self {
            network: SimNetworkProvider::new(sim.clone()),
            time: SimTimeProvider::new(sim),
            task: TokioTaskProvider,
            random: SimRandomProvider::new(seed),
            // TODO: Replace with SimStorageProvider when implemented
            storage: TokioStorageProvider::new(),
        }
    }
}

impl Providers for SimProviders {
    type Network = SimNetworkProvider;
    type Time = SimTimeProvider;
    type Task = TokioTaskProvider;
    type Random = SimRandomProvider;

    fn network(&self) -> &Self::Network {
        &self.network
    }

    fn time(&self) -> &Self::Time {
        &self.time
    }

    fn task(&self) -> &Self::Task {
        &self.task
    }

    fn random(&self) -> &Self::Random {
        &self.random
    }

    // TODO: Replace with SimStorageProvider when implemented
    type Storage = TokioStorageProvider;

    fn storage(&self) -> &Self::Storage {
        &self.storage
    }
}
