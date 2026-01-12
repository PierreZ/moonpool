//! # Moonpool Transport Layer
//!
//! FDB-style transport layer for the moonpool simulation framework.
//!
//! This crate provides networking primitives that work identically in
//! simulation and production environments, following FoundationDB's
//! NetTransport patterns.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │              Application Code                    │
//! │         Uses NetTransport + RPC                 │
//! ├─────────────────────────────────────────────────┤
//! │     NetTransport (endpoint routing)            │
//! │     • Multiplexes connections per endpoint      │
//! │     • Request/response with correlation         │
//! ├─────────────────────────────────────────────────┤
//! │     Peer (connection management)                │
//! │     • Automatic reconnection with backoff       │
//! │     • Message queuing during disconnection      │
//! ├─────────────────────────────────────────────────┤
//! │     Wire Format (serialization)                 │
//! │     • Length-prefixed packets                   │
//! │     • CRC32C checksums                          │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! ## Components
//!
//! | Component | Purpose |
//! |-----------|---------|
//! | [`Peer`] | Resilient connection with automatic reconnection |
//! | [`NetTransport`] | Endpoint routing and connection multiplexing |
//! | [`wire`] | Binary serialization with CRC32C checksums |
//! | [`rpc`] | Request/response patterns with typed messaging |
//!
//! ## Quick Start
//!
//! ```ignore
//! use moonpool_transport::{NetTransportBuilder, send_request};
//!
//! // Build transport with network provider
//! let transport = NetTransportBuilder::new(network_provider, time_provider)
//!     .build();
//!
//! // Send typed request, get typed response
//! let response: PongMessage = send_request(&transport, endpoint, ping).await?;
//! ```

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
    EndpointMap, MessageReceiver, NetNotifiedQueue, NetTransport, NetTransportBuilder, ReplyError,
    ReplyFuture, ReplyPromise, RequestEnvelope, RequestStream, RpcError, method_endpoint,
    method_uid, send_request,
};

// Interface attribute macro
pub use moonpool_transport_derive::interface;
