//! Transport layer abstraction for request-response messaging.
//!
//! This module provides envelope-based messaging with correlation IDs
//! for building request-response semantics on top of raw networking.

/// Transport driver that bridges protocol to actual peer I/O
pub mod driver;
/// Core envelope traits and SimpleEnvelope implementation
pub mod envelope;
/// Sans I/O protocol state machine for transport logic
pub mod protocol;
/// Request-response envelope with binary serialization
pub mod request_response_envelope;
/// Common types and error definitions for transport layer
pub mod types;

pub use driver::*;
pub use envelope::*;
pub use protocol::*;
pub use request_response_envelope::*;
pub use types::*;
