//! Prelude module for common moonpool-foundation imports.
//!
//! This module re-exports the most commonly used types and traits from moonpool-foundation,
//! making it easy to get started without having to remember all the import paths.
//!
//! # Usage
//!
//! ```rust
//! use moonpool_foundation::prelude::*;
//!
//! // Now you have access to all the common types
//! let config = NetworkConfig::default();
//! let sim = SimulationBuilder::new().build();
//! ```

// Core simulation types
pub use crate::error::SimulationResult;
pub use crate::runner::{SimulationBuilder, SimulationReport};
pub use crate::sim::{SimWorld, WeakSimWorld};

// Provider traits
pub use crate::network::NetworkProvider;
pub use crate::random::RandomProvider;
pub use crate::task::TaskProvider;
pub use crate::time::TimeProvider;

// Network configuration and transport
pub use crate::network::PeerConfig;
pub use crate::network::transport::{
    ClientTransport, Envelope, RequestResponseEnvelope, RequestResponseSerializer, ServerTransport,
    SimpleEnvelope, TransportError,
};

// Assertion and testing framework - these are macros exported at the root
pub use crate::{always_assert, buggify, sometimes_assert};

// Common error types
pub use crate::network::transport::types::EnvelopeError;

// Time types
pub use std::time::{Duration, Instant};
