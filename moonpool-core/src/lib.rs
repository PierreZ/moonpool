//! # moonpool-core
//!
//! Core abstractions for the moonpool simulation framework.
//!
//! This crate provides the fundamental traits and types that enable moonpool's
//! simulation capabilities:
//!
//! - **Provider traits**: Abstractions for time, tasks, randomness, and networking
//! - **Core types**: UID, Endpoint, NetworkAddress for FDB-compatible addressing
//! - **Codec trait**: Pluggable message serialization
//!
//! ## Provider Traits
//!
//! The provider traits allow code to work seamlessly with both simulation and
//! real execution environments:
//!
//! - [`TimeProvider`]: Sleep, timeout, and time operations
//! - [`TaskProvider`]: Task spawning for single-threaded environments
//! - [`RandomProvider`]: Deterministic random number generation
//! - [`NetworkProvider`]: Network connection and listener creation
//!
//! ## Core Types
//!
//! FDB-compatible types for endpoint addressing:
//!
//! - [`UID`]: 128-bit unique identifier
//! - [`Endpoint`]: Network address + token for direct addressing
//! - [`NetworkAddress`]: IP address + port
//! - [`WellKnownToken`]: System service tokens

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

mod codec;
mod error;
mod network;
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
pub use random::RandomProvider;
pub use task::{TaskProvider, TokioTaskProvider};
pub use time::{TimeProvider, TokioTimeProvider};

// Core type exports
pub use types::{Endpoint, NetworkAddress, NetworkAddressParseError, UID, flags};
pub use well_known::{WELL_KNOWN_RESERVED_COUNT, WellKnownToken};
