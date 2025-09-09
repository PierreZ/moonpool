//! Transport layer implementation for Moonpool simulation.
//!
//! This module implements a Sans I/O transport layer following the design principles:
//! - Pure state machine for protocol logic
//! - I/O driver for bridging to actual network operations
//! - Separation of concerns for deterministic testing
//!
//! The transport layer provides an abstraction similar to FoundationDB's FlowTransport
//! but designed specifically for deterministic simulation testing.

/// Client transport implementation
pub mod client;
/// I/O driver for bridging protocol to network operations
pub mod driver;
/// Envelope serialization trait and error types
pub mod envelope;
/// NetTransport trait and associated types
pub mod net_transport;
/// Sans I/O protocol state machine
pub mod protocol;
/// Request-response envelope implementation
pub mod request_response_envelope;
/// Server transport implementation
pub mod server;
/// Transport layer types and structures
pub mod types;

pub use client::*;
pub use driver::*;
pub use envelope::*;
pub use net_transport::*;
pub use protocol::*;
pub use request_response_envelope::*;
pub use server::*;
pub use types::*;
