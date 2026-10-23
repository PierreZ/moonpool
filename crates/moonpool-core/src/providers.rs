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
//! Two bundles exist: [`TokioProviders`] here, for production, and
//! `SimProviders` in `moonpool-sim`, which a simulation hands to each process
//! and workload through `SimContext::providers`.

use crate::{
    LocalNetworkProvider, LocalStorageProvider, LocalTaskProvider, LocalTimeProvider,
    NetworkProvider, RandomProvider, StorageProvider, SystemProvider, TaskProvider, TimeProvider,
};
#[cfg(feature = "tokio-providers")]
use crate::{
    TokioNetworkProvider, TokioRandomProvider, TokioStorageProvider, TokioSystemProvider,
    TokioTaskProvider, TokioTimeProvider,
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

/// Local (non-`Send`) bundle: the provider slots a thread-per-core runtime
/// can back, without the task slot.
///
/// The network, time and storage slots take the local traits
/// ([`LocalNetworkProvider`], [`LocalTimeProvider`],
/// [`LocalStorageProvider`]). Random and system keep [`RandomProvider`] and
/// [`SystemProvider`]: their methods are synchronous, so a thread-per-core
/// runtime can back them with the same `Send` types Tokio uses (a
/// thread-local RNG, `getrusage`). The bundle itself need not be `Send`.
///
/// ## No task slot
///
/// Tasks are left out on purpose. A [`TaskProvider`] is not a
/// [`LocalTaskProvider`](crate::LocalTaskProvider) (it cannot spawn a `!Send`
/// future), so a task slot would rule out the blanket impl below, and every
/// existing bundle would need a second, hand-written impl. Without it, every
/// [`Providers`] bundle ([`TokioProviders`], `SimProviders`) is a
/// `LocalProviders` for free. Code that spawns local tasks takes a
/// [`LocalTaskProvider`](crate::LocalTaskProvider) beside the bundle.
///
/// ## Method-name ambiguity
///
/// A concrete [`Providers`] bundle has both traits' accessors under the same
/// names. With both traits in scope, `providers.time()` is ambiguous on a
/// concrete type: import one trait per module, or call
/// `LocalProviders::time(&providers)`. Generic code bounded by one trait is
/// not affected.
pub trait LocalProviders: Clone + 'static {
    /// Network provider type for TCP connections and listeners.
    type Network: LocalNetworkProvider;

    /// Time provider type for sleep, timeout, and time queries.
    type Time: LocalTimeProvider;

    /// Random provider type for deterministic or real randomness.
    type Random: RandomProvider;

    /// Storage provider type for file I/O operations.
    type Storage: LocalStorageProvider;

    /// System provider type for CPU and disk counters.
    type System: SystemProvider;

    /// Get the network provider instance.
    fn network(&self) -> &Self::Network;

    /// Get the time provider instance.
    fn time(&self) -> &Self::Time;

    /// Get the random provider instance.
    fn random(&self) -> &Self::Random;

    /// Get the storage provider instance.
    fn storage(&self) -> &Self::Storage;

    /// Get the system provider instance.
    fn system(&self) -> &Self::System;
}

impl<P: Providers> LocalProviders for P {
    type Network = P::Network;
    type Time = P::Time;
    type Random = P::Random;
    type Storage = P::Storage;
    type System = P::System;

    fn network(&self) -> &Self::Network {
        Providers::network(self)
    }

    fn time(&self) -> &Self::Time {
        Providers::time(self)
    }

    fn random(&self) -> &Self::Random {
        Providers::random(self)
    }

    fn storage(&self) -> &Self::Storage {
        Providers::storage(self)
    }

    fn system(&self) -> &Self::System {
        Providers::system(self)
    }
}

/// A [`LocalProviders`] bundle with a local task slot: what an application
/// on one thread runs on.
///
/// [`LocalProviders`] leaves the task slot out, so that every [`Providers`]
/// bundle is a local bundle for free. An application that spawns `!Send`
/// tasks still needs a spawner beside the bundle. This trait adds it:
///
/// - every [`Providers`] bundle whose task provider is also a
///   [`LocalTaskProvider`] (`SimProviders`, because the simulator runs every
///   task on one thread) implements it with that same task provider;
/// - [`CompioProviders`] (feature `compio-providers`) implements it with
///   [`CompioTaskProvider`](crate::CompioTaskProvider).
///
/// [`TokioProviders`] does not: `tokio::spawn` needs `Send` futures.
///
/// The accessor is named [`local_task`](Self::local_task), not `task`, so a
/// bundle that also implements [`Providers`] never makes a call ambiguous.
///
/// ```rust
/// use moonpool_core::{LocalTaskProvider, ThreadProviders};
///
/// fn start<P: ThreadProviders>(providers: &P) {
///     let shared = std::rc::Rc::new(());
///     drop(providers.local_task().spawn_local_task("rc", async move {
///         let _held = shared;
///     }));
/// }
/// ```
pub trait ThreadProviders: LocalProviders {
    /// Task provider type for spawning `!Send` tasks on the current thread.
    type LocalTask: LocalTaskProvider;

    /// Get the local task provider instance.
    fn local_task(&self) -> &Self::LocalTask;
}

impl<P> ThreadProviders for P
where
    P: Providers<Task: LocalTaskProvider>,
{
    type LocalTask = P::Task;

    fn local_task(&self) -> &Self::LocalTask {
        Providers::task(self)
    }
}

/// Production providers: the six Tokio-backed providers in one bundle.
///
/// | Slot | Type |
/// |------|------|
/// | network | [`TokioNetworkProvider`] |
/// | time | [`TokioTimeProvider`] |
/// | task | [`TokioTaskProvider`] |
/// | random | [`TokioRandomProvider`] |
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
    random: TokioRandomProvider,
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
            random: TokioRandomProvider::new(),
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
    random: TokioRandomProvider,
    storage: TokioStorageProvider,
    system: TokioSystemProvider,
});

/// Compio providers: the local (non-`Send`) production bundle for a
/// thread-per-core application.
///
/// | Slot | Type |
/// |------|------|
/// | network | [`CompioNetworkProvider`](crate::CompioNetworkProvider) |
/// | time | [`CompioTimeProvider`](crate::CompioTimeProvider) |
/// | random | [`TokioRandomProvider`](crate::TokioRandomProvider) (a thread-local RNG; no Tokio involved) |
/// | storage | [`CompioStorageProvider`](crate::CompioStorageProvider) |
/// | system | [`CompioSystemProvider`](crate::CompioSystemProvider) |
///
/// It implements [`LocalProviders`], which has no task slot: spawn with
/// [`CompioTaskProvider`](crate::CompioTaskProvider) beside it. Build it
/// anywhere, and call the async methods from inside a compio runtime, one
/// per thread.
///
/// ```rust
/// use std::time::Duration;
///
/// use moonpool_core::{CompioProviders, LocalProviders, LocalTimeProvider};
///
/// compio::runtime::Runtime::new().expect("runtime").block_on(async {
///     let providers = CompioProviders::new();
///     providers
///         .time()
///         .sleep(Duration::from_millis(1))
///         .await
///         .expect("sleep");
/// });
/// ```
#[cfg(feature = "compio-providers")]
#[derive(Clone, Default)]
pub struct CompioProviders {
    network: crate::CompioNetworkProvider,
    time: crate::CompioTimeProvider,
    random: crate::TokioRandomProvider,
    storage: crate::CompioStorageProvider,
    system: crate::CompioSystemProvider,
}

#[cfg(feature = "compio-providers")]
impl CompioProviders {
    /// Create the compio bundle. Each provider starts with its default
    /// configuration, and the time and system clocks count from now.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(feature = "compio-providers")]
impl LocalProviders for CompioProviders {
    type Network = crate::CompioNetworkProvider;
    type Time = crate::CompioTimeProvider;
    type Random = crate::TokioRandomProvider;
    type Storage = crate::CompioStorageProvider;
    type System = crate::CompioSystemProvider;

    fn network(&self) -> &Self::Network {
        &self.network
    }

    fn time(&self) -> &Self::Time {
        &self.time
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

#[cfg(feature = "compio-providers")]
impl ThreadProviders for CompioProviders {
    type LocalTask = crate::CompioTaskProvider;

    fn local_task(&self) -> &Self::LocalTask {
        &crate::CompioTaskProvider
    }
}
