//! FDB-style static messaging with fixed endpoints.
//!
//! This module implements FoundationDB's proven patterns for endpoint-based messaging:
//!
//! - **EndpointMap**: Token → receiver routing with O(1) well-known lookup
//! - **NetTransport**: Central coordinator managing peers and dispatch
//! - **`NetNotifiedQueue<T>`**: Type-safe message queue with async notification
//!
//! # Design Philosophy
//!
//! Static messaging means endpoints are fixed at compile time or registration time.
//! Well-known endpoints (Ping, EndpointNotFound) use deterministic addressing.
//! Dynamic endpoints are allocated at runtime but still have fixed lifetimes.

mod endpoint_map;
mod net_notified_queue;
mod net_transport;
mod reply_error;
mod reply_future;
mod reply_promise;
mod request;
mod request_stream;

pub use endpoint_map::{EndpointMap, MessageReceiver};
pub use net_notified_queue::{NetNotifiedQueue, RecvFuture, SharedNetNotifiedQueue};
pub use net_transport::{NetTransport, NetTransportBuilder};
pub use reply_error::ReplyError;
pub use reply_future::ReplyFuture;
pub use reply_promise::ReplyPromise;
pub use request::send_request;
pub use request_stream::{RequestEnvelope, RequestStream};
