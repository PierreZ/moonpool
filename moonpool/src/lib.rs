//! Moonpool: FDB-style static messaging for Rust.
//!
//! This crate provides a typed messaging layer on top of moonpool-foundation's
//! raw transport primitives. It implements FoundationDB's proven patterns for
//! reliable network communication.
//!
//! # Architecture
//!
//! ```text
//! moonpool (this crate)
//! ├── messaging/
//! │   └── static/           # FDB-style fixed endpoints
//! │       ├── EndpointMap   # Token → receiver routing
//! │       ├── FlowTransport # Central coordinator
//! │       └── NetNotifiedQueue<T> # Typed message queue
//! │
//! moonpool-foundation
//! └── Raw transport primitives (Peer, wire format, UID, Endpoint)
//! ```
//!
//! # Key Concepts
//!
//! - **EndpointMap**: Routes incoming packets by token to receivers.
//!   Well-known tokens (Ping, EndpointNotFound) use O(1) array lookup.
//!   Dynamic endpoints use HashMap.
//!
//! - **FlowTransport**: Central coordinator managing peer connections and
//!   packet dispatch. Provides synchronous send API (FDB pattern: never await on send).
//!
//! - **NetNotifiedQueue<T>**: Type-safe message queue with async notification.
//!   Deserializes incoming bytes into typed messages.

pub mod error;
pub mod messaging;

// Re-export foundation types for convenience
pub use moonpool_foundation::{
    Endpoint, NetworkAddress, UID, WELL_KNOWN_RESERVED_COUNT, WellKnownToken,
};

// Re-export error types
pub use error::MessagingError;

// Re-export messaging types
pub use messaging::r#static::{
    EndpointMap, FlowTransport, FlowTransportBuilder, MessageReceiver, NetNotifiedQueue,
    RecvFuture, ReplyError, ReplyFuture, ReplyPromise, RequestEnvelope, RequestStream,
    SharedNetNotifiedQueue, send_request,
};
