//! # moonpool-rpc
//!
//! Typed request/reply RPC between dynamically allocated endpoints, written
//! once against the moonpool provider traits so the same code runs over real
//! TCP (`TokioProviders`) and under deterministic simulation
//! (`SimProviders`). Inspired by `FoundationDB`'s `fdbrpc`, without its wire
//! format, global state or Flow runtime.
//!
//! ```text
//!  server runtime                                   client runtime
//!  ┌─────────────────────────────┐   TCP frames     ┌────────────────────────┐
//!  │ RpcDriver (owned, polled)   │◄────────────────►│ RpcDriver              │
//!  │  registry: token → inbox    │                  │  pending: call → reply │
//!  │ RequestStream<M>  ◄─ admit  │                  │ ServiceClient<P, M>    │
//!  │   IncomingRequest ─ reply ─►│                  │   try_get_reply(req)   │
//!  └─────────────────────────────┘                  └────────────────────────┘
//!         ServiceRef<M>  ──── plain bytes, handed out by the application ───►
//! ```
//!
//! ## The manual API
//!
//! 1. Build a runtime with [`RpcDriver::listen`] (or
//!    [`RpcDriver::client_only`]) and poll [`RpcDriver::run`] next to the
//!    application. The driver owns every connection, registration and
//!    pending call; dropping it shuts everything down.
//! 2. Define a method with [`RpcMethod`]: request/reply types implementing
//!    [`Wire`] (any `prost::Message` with the default feature) plus stable
//!    [`MethodId`] / [`SchemaId`] constants.
//! 3. Register with [`RpcHandle::register`]: you get a [`ServiceRef`] (plain,
//!    serialisable routing data: address, transport incarnation, checked
//!    endpoint token, method, schema, codecs) and the owned
//!    [`RequestStream`]. Each [`IncomingRequest`] carries a one-shot
//!    [`ReplyHandle`] bound to the session it arrived on.
//! 4. A caller binds a reference to its own runtime
//!    ([`ServiceRef::bind`]) and calls
//!    [`ServiceClient::try_get_reply`]: one attempt, executed zero or one
//!    times, never retransmitted.
//!
//! ## Contracts
//!
//! - **At most once per attempt.** No hidden retry or reconnect replay. A
//!   failure says what it proves ([`RpcError::execution`]); a timeout,
//!   disconnect after transmission or dropped caller preserves ambiguity.
//! - **Checked identity.** A request is admitted only if its incarnation,
//!   endpoint token (slot + generation), method, schema and codec all match a
//!   live registration, before any body byte is decoded. Dropping a receiver
//!   destroys its endpoint; a reused slot never answers an old token; a
//!   restarted process at the same address rejects its predecessor's
//!   references ([`RpcError::StaleIncarnation`]).
//! - **Peer-relative replies.** A reply handle routes only to the
//!   connection that delivered the request; it has no byte form and cannot
//!   be forwarded.
//! - **Owned lifetimes.** Handles, clients, receivers and reply handles hold
//!   the runtime weakly; nothing but the driver keeps it serving.
//! - **Bounded everything.** Frame size, queued requests, control frames,
//!   pending calls, endpoints and connections all have hard limits
//!   ([`RpcConfig`]); malformed, corrupt or oversized input closes the
//!   connection observably ([`RpcStats`]).
//!
//! Wire format: [`protocol`] (frame and envelope), [`codec`] (bodies).
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

pub mod codec;
mod config;
mod endpoint;
mod error;
pub mod protocol;
mod stats;

pub(crate) mod call;
pub(crate) mod transport;

pub use call::{IncomingRequest, ReplyHandle, RequestStream, ServiceClient, ServiceRef};
pub use codec::{CodecId, DecodeError, EncodeError, Wire};
pub use config::RpcConfig;
pub use endpoint::{Endpoint, EndpointToken, Incarnation};
pub use error::{Execution, RpcError};
pub use protocol::{MethodId, RpcMethod, SchemaId};
pub use stats::{ResourceProbe, RpcStats};
pub use transport::{RpcDriver, RpcHandle};
