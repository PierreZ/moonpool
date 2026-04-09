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

mod delivery;
mod endpoint_map;
mod failure_monitor;
mod fan_out;
mod interface;
mod load_balance;
mod net_notified_queue;
mod net_transport;
mod reply_error;
mod reply_future;
mod reply_promise;
mod request;
mod request_stream;
mod rpc_error;
mod server_handle;
mod service_endpoint;
mod smoother;
#[cfg(test)]
mod test_support;

pub use delivery::{get_reply, get_reply_unless_failed_for, send, try_get_reply};
pub use endpoint_map::{EndpointMap, MessageReceiver};
pub use failure_monitor::{FailedReason, FailureMonitor, FailureStatus};
pub use fan_out::{FanOutError, fan_out_all, fan_out_all_partial, fan_out_quorum, fan_out_race};
pub use interface::{method_endpoint, method_uid};
pub use load_balance::{Alternatives, AtMostOnce, Distance, ModelHolder, QueueModel, load_balance};
pub use net_notified_queue::{NetNotifiedQueue, RecvFuture, SharedNetNotifiedQueue};
pub use net_transport::{NetTransport, NetTransportBuilder};
pub use reply_error::ReplyError;
pub use reply_future::ReplyFuture;
pub use reply_promise::ReplyPromise;
pub use request::send_request;
pub use request_stream::{RequestEnvelope, RequestStream};
pub use rpc_error::RpcError;
pub use server_handle::ServerHandle;
pub use service_endpoint::ServiceEndpoint;
pub use smoother::Smoother;
