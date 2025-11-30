//! FDB-style static messaging with fixed endpoints.
//!
//! This module implements FoundationDB's proven patterns for endpoint-based messaging:
//!
//! - **EndpointMap**: Token → receiver routing with O(1) well-known lookup
//! - **FlowTransport**: Central coordinator managing peers and dispatch
//! - **NetNotifiedQueue<T>**: Type-safe message queue with async notification
//!
//! # Design Philosophy
//!
//! Static messaging means endpoints are fixed at compile time or registration time.
//! Well-known endpoints (Ping, EndpointNotFound) use deterministic addressing.
//! Dynamic endpoints are allocated at runtime but still have fixed lifetimes.

mod endpoint_map;
mod flow_transport;
mod net_notified_queue;

pub use endpoint_map::{EndpointMap, MessageReceiver};
pub use flow_transport::FlowTransport;
pub use net_notified_queue::{NetNotifiedQueue, RecvFuture, SharedNetNotifiedQueue};
