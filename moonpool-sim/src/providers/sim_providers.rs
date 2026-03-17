//! Simulation providers bundle implementation.

use std::net::IpAddr;

use moonpool_core::{Providers, TokioTaskProvider};

use crate::network::SimNetworkProvider;
use crate::sim::WeakSimWorld;
use crate::storage::SimStorageProvider;

use super::{SimRandomProvider, SimTimeProvider};

/// Simulation providers bundle for deterministic testing.
///
/// This struct bundles all simulation-based providers into a single
/// instance that implements [`Providers`]. Each bundle is scoped to a
/// specific process IP for per-process storage fault injection.
///
/// ## Usage
///
/// ```rust,ignore
/// use moonpool_sim::{SimWorld, SimProviders, Providers};
///
/// let sim = SimWorld::new();
/// let ip: std::net::IpAddr = "10.0.1.1".parse().unwrap();
/// let providers = SimProviders::new(sim.downgrade(), 42, ip);
///
/// // Access individual providers
/// let network = providers.network();
/// let storage = providers.storage();
/// ```
///
/// ## Implementation Notes
///
/// - Uses `SimNetworkProvider` for simulated TCP connections
/// - Uses `SimTimeProvider` for logical/simulated time
/// - Uses `TokioTaskProvider` for task spawning (same as production)
/// - Uses `SimRandomProvider` for seeded deterministic randomness
/// - Uses `SimStorageProvider` for simulated file I/O with per-process fault injection
#[derive(Clone)]
pub struct SimProviders {
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task: TokioTaskProvider,
    random: SimRandomProvider,
    storage: SimStorageProvider,
}

impl SimProviders {
    /// Create a new simulation providers bundle scoped to a process IP.
    ///
    /// # Arguments
    ///
    /// * `sim` - Weak reference to the simulation world
    /// * `seed` - Seed for deterministic random number generation
    /// * `ip` - IP address of the owning process (for per-process storage scoping)
    pub fn new(sim: WeakSimWorld, seed: u64, ip: IpAddr) -> Self {
        Self {
            network: SimNetworkProvider::new(sim.clone()),
            time: SimTimeProvider::new(sim.clone()),
            task: TokioTaskProvider,
            random: SimRandomProvider::new(seed),
            storage: SimStorageProvider::new(sim, ip),
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

    type Storage = SimStorageProvider;

    fn storage(&self) -> &Self::Storage {
        &self.storage
    }
}
