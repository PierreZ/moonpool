//! # Moonpool
//!
//! Deterministic simulation framework for distributed systems.
//!
//! This is the main entry point crate that re-exports functionality from:
//! - [`moonpool_core`]: Core types and traits (UID, Endpoint, providers)
//! - [`moonpool_sim`]: Simulation runtime and chaos testing
//! - [`moonpool_transport`]: Network transport and RPC layer
//!
//! ## Future Development
//!
//! This crate will eventually provide the actor runtime experience.

#![deny(missing_docs)]

// Re-export all public items from sub-crates
pub use moonpool_core::*;
pub use moonpool_sim::*;
pub use moonpool_transport::*;
