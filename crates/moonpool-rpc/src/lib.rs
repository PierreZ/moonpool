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
//!    [`Wire`] (any `prost::Message` with the default feature) plus an
//!    explicit `u32` [`MethodId`] and `u16` [`SchemaVersion`].
//! 3. Register with [`RpcHandle::register`] and an [`AccessClass`]: you get
//!    a [`ServiceRef`] (plain, serialisable routing data: resolved address,
//!    128-bit runtime incarnation, checked 64-bit endpoint token, method,
//!    schema, codecs, access class) and the owned [`RequestStream`], the
//!    pull-style receiver. Each [`IncomingRequest`] carries a one-shot
//!    [`ReplyHandle`] bound to the session it arrived on.
//! 4. A caller binds a reference to its own runtime ([`ServiceRef::bind`])
//!    and calls [`ServiceClient::try_get_reply`] (or
//!    [`ServiceClient::try_get_reply_within`] with a deadline): one
//!    attempt, executed zero or one times, never retransmitted.
//!
//! ## Delivery modes
//!
//! | Call | Delivery | Completion |
//! |---|---|---|
//! | [`ServiceClient::send`] | one-way, zero or one executions | `Ok` means queued, never "executed" |
//! | [`ServiceClient::try_get_reply`], [`ServiceClient::attempt`] | one attempt, never retransmitted | reply, or an error that says what it proves |
//! | [`ServiceClient::get_reply`] | request retained in memory and resent on every new connection: **may execute more than once** | first outcome wins; a dead dynamic endpoint ends it at once |
//! | [`ServiceClient::get_reply_unless_failed_for`] | reliable, bounded by sustained observed failure | [`ErrorReason::PeerFailed`] keeps the ambiguity |
//!
//! A [`ReplyAttempt`] is one attempt as a future: kept, it yields a late
//! outcome after its caller moved on; dropped, its route is released and a
//! late reply is counted and discarded. On the serving side a
//! [`ReplyHandle`] can reply, be dropped (a broken promise), or finish with
//! [`ReplyHandle::never_reply`] (nothing sent; the caller's own deadline
//! decides).
//!
//! ## Peers and failure
//!
//! Each remote address has one selected connection, dialed on demand,
//! re-dialed with jittered backoff, pinged while alive and closed when
//! idle ([`PeerPolicy`], plain data). Two listening runtimes that call each
//! other share one connection. The [`FailureMonitor`] keeps address
//! availability, disconnect events and permanent endpoint failure apart,
//! and its waits never lose a wakeup. Hostnames exist only for well-known
//! bootstrap endpoints ([`WellKnownRef`], [`BootstrapClient`] with a
//! [`Resolver`](moonpool_core::Resolver) and an explicit [`RetryPolicy`]);
//! dynamic references are never re-resolved or refreshed.
//!
//! Sessions go through an upgrade seam ([`Connector`] / [`Acceptor`],
//! [`Plaintext`] by default) that yields the session stream and a
//! [`PeerContext`], then a versioned handshake.
//!
//! ## Contracts
//!
//! - **At most once per attempt.** No hidden retry or reconnect replay
//!   outside [`ServiceClient::get_reply`], which says so. Every failure is one [`RpcError`]: a [reason](ErrorReason) plus what it
//!   proves ([`Execution::NotAdmitted`], [`Execution::MaybeExecuted`],
//!   [`Execution::Executed`]); a timeout, disconnect after transmission or
//!   dropped caller preserves ambiguity.
//! - **Checked identity.** A request is admitted only if its incarnation,
//!   endpoint token (slot + generation), method, schema and codec all match a
//!   live registration, before any body byte is decoded. Dropping a receiver
//!   destroys its endpoint; a reused slot never answers an old token; a
//!   restarted process at the same address rejects its predecessor's
//!   references ([`ErrorReason::StaleIncarnation`]).
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
mod failure;
pub mod protocol;
mod stats;

pub(crate) mod call;
pub(crate) mod transport;

pub use call::{
    BootstrapAddress, BootstrapClient, BootstrapStats, IncomingRequest, ReplyAttempt, ReplyHandle,
    RequestStream, RetryPolicy, ServiceClient, ServiceRef, WellKnownRef,
};
pub use codec::{CodecId, DecodeError, EncodeError, Wire};
pub use config::{
    InboundSharing, InvalidConfig, MAX_FRAME_BYTES, MIN_FRAME_BYTES, PeerPolicy, RpcConfig,
};
pub use endpoint::{AccessClass, Endpoint, EndpointToken, Incarnation, WellKnownId};
pub use error::{ErrorReason, Execution, RpcError};
pub use failure::{AddressState, EndpointState, FailureMonitor};
pub use protocol::{MethodId, RpcMethod, SchemaVersion};
pub use stats::{ResourceProbe, RpcStats};
pub use transport::upgrade::{Acceptor, Connector, PeerContext, Plaintext};
pub use transport::{RpcDriver, RpcHandle, SessionUpgrade, is_transient_accept_error};
