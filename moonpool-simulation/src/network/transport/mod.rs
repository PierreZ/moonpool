//! Transport layer abstraction for request-response messaging.
//!
//! This module provides envelope-based messaging with correlation IDs
//! for building request-response semantics on top of raw networking.

/// Core envelope traits and SimpleEnvelope implementation
pub mod envelope;
/// Common types and error definitions for transport layer
pub mod types;

pub use envelope::*;
pub use types::*;
