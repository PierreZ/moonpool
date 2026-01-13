//! Provider bundle trait for simplified type parameters.
//!
//! This module provides a unified [`Providers`] trait that bundles all four
//! provider types into a single type parameter, eliminating type parameter
//! explosion in downstream code.
//!
//! ## Motivation
//!
//! Without bundling, code must carry four separate type parameters:
//!
//! ```text
//! struct MyStruct<N, T, TP, R>
//! where
//!     N: NetworkProvider + Clone + 'static,
//!     T: TimeProvider + Clone + 'static,
//!     TP: TaskProvider + Clone + 'static,
//!     R: RandomProvider + Clone + 'static,
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
    NetworkProvider, RandomProvider, TaskProvider, TimeProvider, TokioNetworkProvider,
    TokioRandomProvider, TokioTaskProvider, TokioTimeProvider,
};

/// Bundle of all provider types for a runtime environment.
///
/// This trait consolidates the four provider types ([`NetworkProvider`],
/// [`TimeProvider`], [`TaskProvider`], [`RandomProvider`]) into a single
/// bundle, reducing type parameter explosion and repetitive where clauses.
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
pub trait Providers: Clone + 'static {
    /// Network provider type for TCP connections and listeners.
    type Network: NetworkProvider + Clone + 'static;

    /// Time provider type for sleep, timeout, and time queries.
    type Time: TimeProvider + Clone + 'static;

    /// Task provider type for spawning local tasks.
    type Task: TaskProvider + Clone + 'static;

    /// Random provider type for deterministic or real randomness.
    type Random: RandomProvider + Clone + 'static;

    /// Get the network provider instance.
    fn network(&self) -> &Self::Network;

    /// Get the time provider instance.
    fn time(&self) -> &Self::Time;

    /// Get the task provider instance.
    fn task(&self) -> &Self::Task;

    /// Get the random provider instance.
    fn random(&self) -> &Self::Random;
}

/// Production providers using Tokio runtime.
///
/// This struct bundles all four Tokio-based providers into a single
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
#[derive(Clone)]
pub struct TokioProviders {
    network: TokioNetworkProvider,
    time: TokioTimeProvider,
    task: TokioTaskProvider,
    random: TokioRandomProvider,
}

impl TokioProviders {
    /// Create a new production providers bundle.
    ///
    /// Initializes all four Tokio-based providers with their default
    /// configurations.
    pub fn new() -> Self {
        Self {
            network: TokioNetworkProvider::new(),
            time: TokioTimeProvider::new(),
            task: TokioTaskProvider,
            random: TokioRandomProvider::new(),
        }
    }
}

impl Default for TokioProviders {
    fn default() -> Self {
        Self::new()
    }
}

impl Providers for TokioProviders {
    type Network = TokioNetworkProvider;
    type Time = TokioTimeProvider;
    type Task = TokioTaskProvider;
    type Random = TokioRandomProvider;

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
}
