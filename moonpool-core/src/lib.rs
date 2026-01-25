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
//! | [`TimeProvider`] | Logical time | Wall clock | Sleep, timeout, now() |
//! | [`TaskProvider`] | Event-driven | Tokio spawn | Task spawning |
//! | [`RandomProvider`] | Seeded RNG | System RNG | Deterministic randomness |
//! | [`NetworkProvider`] | Simulated TCP | Real TCP | Connect, listen, accept |
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
mod task;
mod time;
mod types;
mod well_known;

// Codec exports
pub use codec::{CodecError, JsonCodec, MessageCodec};

// Error exports
pub use error::{SimulationError, SimulationResult};

// Provider trait exports
pub use network::{NetworkProvider, TcpListenerTrait, TokioNetworkProvider, TokioTcpListener};
pub use providers::{Providers, TokioProviders};
pub use random::{RandomProvider, TokioRandomProvider};
pub use task::{TaskProvider, TokioTaskProvider};
pub use time::{TimeProvider, TokioTimeProvider};

// Core type exports
pub use types::{Endpoint, NetworkAddress, NetworkAddressParseError, UID, flags};
pub use well_known::{WELL_KNOWN_RESERVED_COUNT, WellKnownToken};
