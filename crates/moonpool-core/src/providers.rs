//! Provider bundle trait for simplified type parameters.
//!
//! This module provides a unified [`Providers`] trait that bundles all six
//! provider types into a single type parameter, eliminating type parameter
//! explosion in downstream code.
//!
//! ## Motivation
//!
//! Without bundling, code must carry six separate type parameters:
//!
//! ```text
//! struct MyStruct<N, T, TP, R, S, Y>
//! where
//!     N: NetworkProvider,
//!     T: TimeProvider,
//!     TP: TaskProvider,
//!     R: RandomProvider,
//!     S: StorageProvider,
//!     Y: SystemProvider,
//! ```
//!
//! With bundling, this simplifies to:
//!
//! ```text
//! struct MyStruct<P: Providers>
//! ```
//!
//! ## Usage
//!
//! ```rust,ignore
//! use moonpool_core::{Providers, TokioProviders};
//!
//! let providers = TokioProviders::new();
//! let time_now = providers.time().now();
//! ```

use crate::{
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
/// - [`TokioProviders`]: Production providers using real Tokio runtime
/// - `SimProviders` (in moonpool-sim): Simulation providers for deterministic testing
///
/// ## Design
///
/// The trait uses associated types to preserve type information at compile time
/// without runtime dispatch. Accessor methods provide convenient access to
/// individual providers while maintaining the bundle.
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

/// Production providers using Tokio runtime.
///
/// This struct bundles all six Tokio-based providers into a single
/// instance that implements [`Providers`].
///
/// ## Example
///
/// ```rust,ignore
/// use moonpool_core::{Providers, TokioProviders};
///
/// let providers = TokioProviders::new();
///
/// // Access individual providers
/// let network = providers.network();
/// let time = providers.time();
/// let task = providers.task();
/// let random = providers.random();
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
    /// Create a new production providers bundle.
    ///
    /// Initializes all six Tokio-based providers with their default
    /// configurations.
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
