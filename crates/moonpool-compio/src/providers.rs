//! [`CompioProviders`]: the six compio-backed providers in one bundle.

use moonpool_core::ThreadRngRandomProvider;

use crate::{
    CompioNetworkProvider, CompioStorageProvider, CompioSystemProvider, CompioTaskProvider,
    CompioTimeProvider,
};

/// Production providers for a thread-per-core deployment on compio.
///
/// | Slot | Type |
/// |------|------|
/// | network | [`CompioNetworkProvider`] |
/// | time | [`CompioTimeProvider`] |
/// | task | [`CompioTaskProvider`] |
/// | random | `ThreadRngRandomProvider` |
/// | storage | [`CompioStorageProvider`] |
/// | system | [`CompioSystemProvider`] |
///
/// The random slot is `moonpool_core::ThreadRngRandomProvider`, rand's
/// thread-local generator, shared by every production bundle.
///
/// The task provider implements
/// [`LocalTaskProvider`](moonpool_core::LocalTaskProvider), so this bundle is
/// a [`LocalProviders`](moonpool_core::LocalProviders) bundle. Build it
/// anywhere; call the async methods from inside a compio runtime.
#[derive(Clone)]
pub struct CompioProviders {
    network: CompioNetworkProvider,
    time: CompioTimeProvider,
    task: CompioTaskProvider,
    random: ThreadRngRandomProvider,
    storage: CompioStorageProvider,
    system: CompioSystemProvider,
}

impl CompioProviders {
    /// Create the bundle. The time and system clocks count from now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            network: CompioNetworkProvider::new(),
            time: CompioTimeProvider::new(),
            task: CompioTaskProvider::new(),
            random: ThreadRngRandomProvider::new(),
            storage: CompioStorageProvider::new(),
            system: CompioSystemProvider::new(),
        }
    }
}

impl Default for CompioProviders {
    fn default() -> Self {
        Self::new()
    }
}

moonpool_core::impl_providers_bundle!(CompioProviders {
    network: CompioNetworkProvider,
    time: CompioTimeProvider,
    task: CompioTaskProvider,
    random: ThreadRngRandomProvider,
    storage: CompioStorageProvider,
    system: CompioSystemProvider,
});
