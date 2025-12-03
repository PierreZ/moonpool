//! # Moonpool Transport Layer
//!
//! FDB-style transport layer for the moonpool framework.
//!
//! This crate provides:
//! - **Peer**: Resilient connection management with automatic reconnection
//! - **Wire format**: FDB-compatible packet serialization with CRC32C
//! - **FlowTransport**: Endpoint routing and message dispatch
//! - **RPC primitives**: Request/response patterns with typed messaging

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

// Re-export core types for convenience
pub use moonpool_core::{
    CodecError, Endpoint, JsonCodec, MessageCodec, NetworkAddress, NetworkAddressParseError,
    NetworkProvider, RandomProvider, SimulationError, SimulationResult, TaskProvider,
    TcpListenerTrait, TimeProvider, TokioNetworkProvider, TokioTaskProvider, TokioTimeProvider,
    UID, WELL_KNOWN_RESERVED_COUNT, WellKnownToken,
};

// =============================================================================
// Modules
// =============================================================================

/// Error types for transport operations.
pub mod error;

/// Resilient peer connection management.
pub mod peer;

/// FDB-compatible wire format with CRC32C checksums.
pub mod wire;

/// RPC layer with typed request/response patterns.
pub mod rpc;

// =============================================================================
// Public API Re-exports
// =============================================================================

// Error exports
pub use error::MessagingError;

// Peer exports
pub use peer::{Peer, PeerConfig, PeerError, PeerMetrics, PeerReceiver};

// Wire format exports
pub use wire::{
    HEADER_SIZE, MAX_PAYLOAD_SIZE, PacketHeader, WireError, deserialize_packet, serialize_packet,
    try_deserialize_packet,
};

// RPC exports
pub use rpc::{
    EndpointMap, FlowTransport, FlowTransportBuilder, MessageReceiver, NetNotifiedQueue,
    ReplyError, ReplyFuture, ReplyPromise, RequestEnvelope, RequestStream, send_request,
};
