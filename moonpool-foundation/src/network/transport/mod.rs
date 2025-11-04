//! Transport layer abstraction for request-response messaging.
//!
//! This module provides envelope-based messaging with correlation IDs
//! for building request-response semantics on top of raw networking.

/// Client transport for request-response messaging
pub mod client;
/// Convenience type aliases for common transport configurations
pub mod convenience;
/// Transport driver that bridges protocol to actual peer I/O
pub mod driver;
/// Core envelope traits and SimpleEnvelope implementation
pub mod envelope;
/// Sans I/O protocol state machine for transport logic
pub mod protocol;
/// Request-response envelope with binary serialization
pub mod request_response_envelope;
/// Server transport for accepting connections and handling requests
pub mod server;
/// Common types and error definitions for transport layer
pub mod types;

pub use client::*;
pub use convenience::*;
pub use driver::*;
pub use envelope::{Envelope, SimpleEnvelope};
pub use protocol::*;
pub use request_response_envelope::{RequestResponseEnvelope, RequestResponseSerializer};
pub use server::*;
pub use types::*;
