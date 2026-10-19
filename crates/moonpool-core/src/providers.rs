//! The [`Providers`] bundle: six provider types behind one type parameter.
//!
//! Without a bundle, a component that needs every provider carries six type
//! parameters:
//!
//! ```text
//! struct Node<N, T, TP, R, S, Y>
//! where
//!     N: NetworkProvider,
//!     T: TimeProvider,
//!     TP: TaskProvider,
//!     R: RandomProvider,
//!     S: StorageProvider,
//!     Y: SystemProvider,
//! ```
//!
//! With the bundle it carries one, and reaches each provider through an
//! accessor:
//!
//! ```rust
//! use moonpool_core::prelude::*;
//!
//! struct Node<P: Providers> {
//!     providers: P,
//! }
//!
//! impl<P: Providers> Node<P> {
//!     fn uptime(&self) -> std::time::Duration {
//!         self.providers.time().now()
//!     }
//! }
//!
//! let node = Node { providers: TokioProviders::new() };
//! let _ = node.uptime();
//! ```
//!
//! Three bundles exist: [`TokioProviders`] here, for production on tokio's
//! work-stealing runtime; [`TokioLocalProviders`] here, for production on
//! tokio's local flavors, where tasks may hold `!Send` state; and
//! `SimProviders` in `moonpool-sim`, which a simulation hands to each process
//! and workload through `SimContext::providers`.
//!
//! Code that spawns `!Send` futures bounds on [`LocalProviders`] instead of
//! [`Providers`]: it accepts the bundles whose task provider implements
//! [`LocalTaskProvider`], and rejects [`TokioProviders`] at compile time.

use crate::{
    LocalTaskProvider, NetworkProvider, RandomProvider, StorageProvider, SystemProvider,
    TaskProvider, TimeProvider,
};
#[cfg(feature = "tokio-providers")]
use crate::{
    ThreadRngRandomProvider, TokioLocalTaskProvider, TokioNetworkProvider, TokioStorageProvider,
    TokioSystemProvider, TokioTaskProvider, TokioTimeProvider,
};

/// Implement [`Providers`] for a struct whose fields are named
/// `network`, `time`, `task`, `random`, `storage`, `system` — the six
/// canonical provider slots.
///
/// Expands to the trait impl plus the six trivial getter forwards. The
/// type of each slot is specified once in the macro invocation, so the
/// implementing struct does not have to repeat the field-to-type mapping
/// in the trait body.
#[doc(hidden)]
#[macro_export]
macro_rules! impl_providers_bundle {
    (
        $struct:ty {
            network: $network_ty:ty,
            time: $time_ty:ty,
            task: $task_ty:ty,
            random: $random_ty:ty,
            storage: $storage_ty:ty,
            system: $system_ty:ty $(,)?
        }
    ) => {
        impl $crate::Providers for $struct {
            type Network = $network_ty;
            type Time = $time_ty;
            type Task = $task_ty;
            type Random = $random_ty;
            type Storage = $storage_ty;
            type System = $system_ty;

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

            fn storage(&self) -> &Self::Storage {
                &self.storage
            }

            fn system(&self) -> &Self::System {
                &self.system
            }
        }
    };
}

/// Bundle of all provider types for a runtime environment.
///
/// This trait consolidates the six provider types ([`NetworkProvider`],
/// [`TimeProvider`], [`TaskProvider`], [`RandomProvider`], [`StorageProvider`],
/// [`SystemProvider`])
/// into a single bundle, reducing type parameter explosion and repetitive
/// where clauses.
///
/// ## Implementations
///
/// - [`TokioProviders`]: production providers on a real Tokio runtime.
/// - [`TokioLocalProviders`]: production providers on a Tokio local runtime,
///   whose task provider also spawns `!Send` futures.
/// - `SimProviders` (in `moonpool-sim`): deterministic simulation providers.
///
/// ## Design
///
/// Associated types keep every provider type known at compile time, so there
/// is no dynamic dispatch. The bundle is `Clone + Send + Sync + 'static`, like
/// each provider, so it moves into spawned tasks.
pub trait Providers: Clone + Send + Sync + 'static {
    /// Network provider type for TCP connections and listeners.
    type Network: NetworkProvider;

    /// Time provider type for sleep, timeout, and time queries.
    type Time: TimeProvider;

    /// Task provider type for spawning `Send` tasks.
    type Task: TaskProvider;

    /// Random provider type for deterministic or real randomness.
    type Random: RandomProvider;

    /// Storage provider type for file I/O operations.
    type Storage: StorageProvider;

    /// System provider type for CPU and disk counters.
    type System: SystemProvider;

    /// Get the network provider instance.
    fn network(&self) -> &Self::Network;

    /// Get the time provider instance.
    fn time(&self) -> &Self::Time;

    /// Get the task provider instance.
    fn task(&self) -> &Self::Task;

    /// Get the random provider instance.
    fn random(&self) -> &Self::Random;

    /// Get the storage provider instance.
    fn storage(&self) -> &Self::Storage;

    /// Get the system provider instance.
    fn system(&self) -> &Self::System;
}

/// A [`Providers`] bundle whose task provider can also spawn `!Send` futures.
///
/// Bound on `P: LocalProviders` instead of `P: Providers` in thread-per-core
/// code that holds `Rc<RefCell<…>>` across awaits and spawns it with
/// `providers.task().spawn_local(..)`. Every bundle whose
/// [`Task`](Providers::Task) implements [`LocalTaskProvider`] gets this trait
/// through the blanket impl; there is nothing to implement by hand.
///
/// [`TokioLocalProviders`] satisfies it. [`TokioProviders`] does not: its
/// task provider is the work-stealing `tokio::spawn`, which moves tasks
/// between threads.
pub trait LocalProviders: Providers<Task: LocalTaskProvider> {}

impl<P: Providers> LocalProviders for P where P::Task: LocalTaskProvider {}

/// Production providers: the six Tokio-backed providers in one bundle.
///
/// | Slot | Type |
/// |------|------|
/// | network | [`TokioNetworkProvider`] |
/// | time | [`TokioTimeProvider`] |
/// | task | [`TokioTaskProvider`] |
/// | random | [`ThreadRngRandomProvider`] |
/// | storage | [`TokioStorageProvider`] |
/// | system | [`TokioSystemProvider`] |
///
/// Tasks and timers need a running Tokio runtime. Build the bundle anywhere,
/// and call the async methods from inside the runtime.
///
/// ```rust
/// use moonpool_core::prelude::*;
///
/// let providers = TokioProviders::new();
/// let _elapsed = providers.time().now();
/// let _coin = providers.random().random_bool(0.5);
/// ```
#[cfg(feature = "tokio-providers")]
#[derive(Clone)]
pub struct TokioProviders {
    network: TokioNetworkProvider,
    time: TokioTimeProvider,
    task: TokioTaskProvider,
    random: ThreadRngRandomProvider,
    storage: TokioStorageProvider,
    system: TokioSystemProvider,
}

#[cfg(feature = "tokio-providers")]
impl TokioProviders {
    /// Create the production bundle. Each provider starts with its default
    /// configuration, and the time and system clocks count from now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            network: TokioNetworkProvider::new(),
            time: TokioTimeProvider::new(),
            task: TokioTaskProvider,
            random: ThreadRngRandomProvider::new(),
            storage: TokioStorageProvider::new(),
            system: TokioSystemProvider::new(),
        }
    }
}

#[cfg(feature = "tokio-providers")]
impl Default for TokioProviders {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "tokio-providers")]
impl_providers_bundle!(TokioProviders {
    network: TokioNetworkProvider,
    time: TokioTimeProvider,
    task: TokioTaskProvider,
    random: ThreadRngRandomProvider,
    storage: TokioStorageProvider,
    system: TokioSystemProvider,
});

/// Production providers for tokio's local flavors: [`TokioProviders`] with a
/// task provider that also spawns `!Send` futures.
///
/// | Slot | Type |
/// |------|------|
/// | network | [`TokioNetworkProvider`] |
/// | time | [`TokioTimeProvider`] |
/// | task | [`TokioLocalTaskProvider`] |
/// | random | [`ThreadRngRandomProvider`] |
/// | storage | [`TokioStorageProvider`] |
/// | system | [`TokioSystemProvider`] |
///
/// It implements [`LocalProviders`], so thread-per-core code bounded on
/// `P: LocalProviders` runs on it. `spawn_local` must be called inside a
/// `tokio::task::LocalSet` (stable) or on a `tokio::runtime::LocalRuntime`
/// (`--cfg tokio_unstable`, `#[tokio::main(flavor = "local")]`); tokio panics
/// with its own message otherwise. The other slots behave exactly as in
/// [`TokioProviders`].
///
/// ```rust
/// use moonpool_core::prelude::*;
///
/// let providers = TokioLocalProviders::new();
/// let _elapsed = providers.time().now();
/// ```
#[cfg(feature = "tokio-providers")]
#[derive(Clone)]
pub struct TokioLocalProviders {
    network: TokioNetworkProvider,
    time: TokioTimeProvider,
    task: TokioLocalTaskProvider,
    random: ThreadRngRandomProvider,
    storage: TokioStorageProvider,
    system: TokioSystemProvider,
}

#[cfg(feature = "tokio-providers")]
impl TokioLocalProviders {
    /// Create the local production bundle. Each provider starts with its
    /// default configuration, and the time and system clocks count from now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            network: TokioNetworkProvider::new(),
            time: TokioTimeProvider::new(),
            task: TokioLocalTaskProvider,
            random: ThreadRngRandomProvider::new(),
            storage: TokioStorageProvider::new(),
            system: TokioSystemProvider::new(),
        }
    }
}

#[cfg(feature = "tokio-providers")]
impl Default for TokioLocalProviders {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "tokio-providers")]
impl_providers_bundle!(TokioLocalProviders {
    network: TokioNetworkProvider,
    time: TokioTimeProvider,
    task: TokioLocalTaskProvider,
    random: ThreadRngRandomProvider,
    storage: TokioStorageProvider,
    system: TokioSystemProvider,
});

#[cfg(all(test, feature = "tokio-providers"))]
mod tests {
    use super::{LocalProviders, TokioLocalProviders};

    fn needs_local<P: LocalProviders>(_: &P) {}

    // `TokioProviders` intentionally does not satisfy `LocalProviders`: its
    // task provider is the work-stealing `tokio::spawn`.
    #[test]
    fn tokio_local_providers_is_local() {
        needs_local(&TokioLocalProviders::new());
    }
}
