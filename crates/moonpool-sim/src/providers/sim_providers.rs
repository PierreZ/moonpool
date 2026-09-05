//! Simulation providers bundle implementation.

use std::net::IpAddr;

use moonpool_core::impl_providers_bundle;

use crate::network::SimNetworkProvider;
use crate::sim::WeakSimWorld;
use crate::storage::SimStorageProvider;

use super::{SimRandomProvider, SimTaskProvider, SimTimeProvider};

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
/// let providers = SimProviders::new(sim.downgrade(), ip);
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
/// - Uses `SimTaskProvider` for task spawning on the deterministic executor
/// - Uses `SimRandomProvider` for the simulation's seeded thread-local
///   randomness (seeded once per run by the runner or by `SimWorld`
///   construction; building a bundle never reseeds it)
/// - Uses `SimStorageProvider` for simulated file I/O with per-process fault injection
#[derive(Clone)]
pub struct SimProviders {
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task: SimTaskProvider,
    random: SimRandomProvider,
    storage: SimStorageProvider,
    sim: WeakSimWorld,
    ip: IpAddr,
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
            task: SimTaskProvider,
            random: SimRandomProvider::new(),
            storage: SimStorageProvider::new(sim.clone(), ip),
            sim,
            ip,
        }
    }

    /// The simulated block-device provider scoped to this bundle's process IP.
    ///
    /// This sits outside the [`Providers`](moonpool_core::Providers) trait —
    /// code generic over a block device should take a
    /// [`BlockDeviceProvider`](moonpool_core::BlockDeviceProvider) bound of
    /// its own (`TokioBlockDeviceProvider` in production).
    ///
    /// # Panics
    ///
    /// Panics if the simulation has already shut down.
    #[must_use]
    pub fn block_devices(&self) -> crate::storage::SimBlockDeviceProvider {
        self.sim
            .upgrade()
            .expect("simulation shut down before block device provider access")
            .block_device_provider(self.ip)
    }
}

impl_providers_bundle!(SimProviders {
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task: SimTaskProvider,
    random: SimRandomProvider,
    storage: SimStorageProvider,
});
