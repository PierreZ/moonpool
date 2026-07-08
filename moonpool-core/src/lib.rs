//! # moonpool-core
//!
//! Core abstractions for the moonpool simulation framework.
//!
//! This crate provides the foundational traits and types that enable
//! moonpool's simulation capabilities. Application code depends on
//! these abstractions rather than concrete implementations, allowing
//! seamless switching between simulation and production environments.
//!
//! ## The Provider Pattern
//!
//! The key insight is that distributed systems interact with the outside
//! world through a small set of operations: time, networking, task spawning,
//! and randomness. By abstracting these behind traits, we can substitute
//! deterministic simulation implementations during testing.
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │                 Application Code                      │
//! │     Uses: TimeProvider, NetworkProvider, etc.         │
//! └───────────────────────┬──────────────────────────────┘
//!                         │ depends on traits
//!          ┌──────────────┴──────────────┐
//!          ▼                             ▼
//!   ┌─────────────────┐         ┌─────────────────┐
//!   │   Simulation    │         │   Production    │
//!   │ SimTimeProvider │         │ TokioTimeProvider│
//!   │ SimNetworkProv. │         │ TokioNetworkProv.│
//!   │ (deterministic) │         │  (real I/O)     │
//!   └─────────────────┘         └─────────────────┘
//! ```
//!
//! ## Provider Traits
//!
//! | Trait | Simulation | Production | Purpose |
//! |-------|------------|------------|---------|
//! | [`TimeProvider`] | Logical time | Wall clock | Sleep, timeout, `now()` |
//! | [`TaskProvider`] | Event-driven | Tokio spawn | Task spawning |
//! | [`RandomProvider`] | Seeded RNG | System RNG | Deterministic randomness |
//! | [`NetworkProvider`] | Simulated TCP | Real TCP | Connect, listen, accept |
//! | [`StorageProvider`] | Fault-injected I/O | Real filesystem | File open, read, write, sync |
//!
//! **Important**: Never call tokio directly in application code.
//! - ❌ `tokio::time::sleep()`
//! - ✅ `time_provider.sleep()`
//!
//! ## Core Types
//!
//! Types for endpoint addressing:
//!
//! - [`UID`]: 128-bit unique identifier (deterministically generated in simulation)
//! - [`Endpoint`]: Network address + token for direct addressing
//! - [`NetworkAddress`]: IP address + port
//! - [`WellKnownToken`]: Reserved tokens for system services

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

mod codec;
mod error;
mod network;
mod providers;
mod random;
#[cfg(feature = "deterministic-select")]
mod select;
#[cfg(feature = "deterministic-select")]
pub mod select_support;
mod storage;
mod task;
mod time;
mod types;
mod well_known;

/// `select!` as tokio's macro, verbatim (production passthrough).
///
/// With the `deterministic-select` feature enabled (moonpool-sim does this),
/// this re-export is replaced by the seeded rotation combinator defined in
/// [`select`](crate::select!); the grammar is identical either way.
#[cfg(all(feature = "select", not(feature = "deterministic-select")))]
pub use tokio::select;

/// tokio re-export used by `select!` expansions so downstream crates never
/// need a direct tokio dependency for the macro to resolve. Not public API.
#[cfg(feature = "select")]
#[doc(hidden)]
pub use tokio as __tokio;

// Codec exports
pub use codec::{CodecError, JsonCodec, MessageCodec};

// Error exports
pub use error::{SimulationError, SimulationResult};

// Provider trait exports
pub use network::{NetworkProvider, TcpListenerTrait};
#[cfg(feature = "tokio-net")]
pub use network::{TokioNetworkProvider, TokioTcpListener};
pub use providers::Providers;
#[cfg(feature = "tokio-providers")]
pub use providers::TokioProviders;
pub use random::RandomProvider;
#[cfg(feature = "tokio-random")]
pub use random::TokioRandomProvider;
pub use storage::{OpenOptions, StorageFile, StorageProvider};
#[cfg(feature = "tokio-fs")]
pub use storage::{TokioStorageFile, TokioStorageProvider};
pub use task::{JoinError, TaskProvider};
#[cfg(feature = "tokio-task")]
pub use task::{TokioJoinHandle, TokioTaskProvider};
#[cfg(feature = "tokio-time")]
pub use time::TokioTimeProvider;
pub use time::{TimeError, TimeProvider};

// Core type exports
pub use types::{Endpoint, NetworkAddress, NetworkAddressParseError, UID, flags};
pub use well_known::{WELL_KNOWN_RESERVED_COUNT, WellKnownToken};

/// Common imports for writing code against the provider traits.
///
/// Bringing the provider traits into scope is the usual ergonomic need — their
/// `async fn`s (RPIT) are only callable with the trait in scope. Glob-import this
/// in production code:
///
/// ```
/// use moonpool_core::prelude::*;
/// ```
pub mod prelude {
    pub use crate::{
        NetworkProvider, Providers, RandomProvider, StorageProvider, TaskProvider,
        TcpListenerTrait, TimeProvider,
    };

    #[cfg(feature = "tokio-providers")]
    pub use crate::TokioProviders;
}
