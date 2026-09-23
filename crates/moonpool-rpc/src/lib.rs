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
//!   ServiceRef<M> / InterfaceRef<I> ── protobuf data, handed out by the app ──►
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
//! ## Interfaces as data
//!
//! A service with several methods is an endpoint **group**
//! ([`RpcHandle::register_group`], [`ServiceGroup`]): one slot, one
//! incarnation, methods told apart by their explicit ids, named by an
//! [`RpcInterface`] with its own [`InterfaceId`]. Its [`InterfaceRef`] and
//! every [`ServiceRef`] are protobuf messages: embed them in requests and
//! replies, store them, forward them to a third participant; decoding
//! needs no runtime and keeps nothing alive. A restarted process publishes
//! fresh references and callers learn them explicitly; a reference to the
//! previous incarnation is refused, never redirected, and a reference to
//! another interface is refused by the server before decoding. See
//! [`interface`].
//!
//! The optional `derive` feature adds `#[moonpool_rpc::service]`, which
//! generates the interface and method markers, a dispatcher over the
//! request streams and a typed client from one trait. It is built on the
//! manual API above and owns no protocol state; the manual API stays
//! first-class.
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
//! [`balance`] spreads calls of one method over an explicit, versioned set
//! of alternatives (locality, a queue model, penalties, temporary
//! exclusion), with a retry permission and a duplicate (hedge) permission
//! kept apart and both off by default.
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
//!   be forwarded (a forwardable callback is an ordinary registered
//!   endpoint whose [`ServiceRef`] travels in the request):
//!
//!   ```compile_fail
//!   # use moonpool_rpc::{MethodId, ReplyHandle, RpcMethod, SchemaVersion, Wire};
//!   # struct Echo;
//!   # impl RpcMethod for Echo {
//!   #     type Request = String;
//!   #     type Reply = String;
//!   #     const METHOD: MethodId = MethodId::new(1);
//!   #     const SCHEMA: SchemaVersion = SchemaVersion::new(1);
//!   #     const NAME: &'static str = "echo";
//!   # }
//!   fn embed<T: Wire>() {}
//!   embed::<ReplyHandle<Echo>>(); // a reply handle is not data
//!   ```
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

pub mod balance;
pub mod codec;
mod config;
mod endpoint;
mod error;
mod failure;
pub mod interface;
pub mod protocol;
mod stats;
pub mod stream;

pub(crate) mod call;
pub(crate) mod transport;

pub use call::{
    BootstrapAddress, BootstrapClient, BootstrapStats, IncomingRequest, ReplyAttempt, ReplyHandle,
    RequestStream, RetryPolicy, ServiceClient, WellKnownRef,
};
pub use codec::{CodecId, DecodeError, EncodeError, Wire};
pub use config::{
    InboundSharing, InvalidConfig, MAX_FRAME_BYTES, MIN_FRAME_BYTES, PeerPolicy, ResourceLimits,
    RpcConfig, StreamPolicy,
};
pub use endpoint::{AccessClass, Endpoint, EndpointToken, Incarnation, WellKnownId};
pub use error::{ErrorReason, Execution, RpcError};
pub use failure::{AddressState, EndpointState, FailureMonitor};
pub use interface::{
    InterfaceClient, InterfaceId, InterfaceMethod, InterfaceRef, RpcInterface, ServiceGroup,
    ServiceRef,
};
pub use protocol::{MethodId, RpcMethod, SchemaVersion};
pub use stats::{ResourceProbe, RpcStats};
pub use stream::{ReplyStream, SendError, StreamProducer};
pub use transport::upgrade::{Acceptor, Connector, PeerContext, Plaintext};
pub use transport::{RpcDriver, RpcHandle, SessionUpgrade, is_transient_accept_error};

/// Generate a typed interface from a trait (feature `derive`).
///
/// ```
/// # #[cfg(feature = "prost")] {
/// #[moonpool_rpc::service(id = 0x6b76_0001, version = 1)]
/// pub trait Kv {
///     /// Read a key.
///     #[method(id = 1, schema = 1)]
///     async fn get(&self, key: String) -> String;
///     /// Write a key; nothing to answer.
///     #[method(id = 2, schema = 1)]
///     async fn put(&self, entry: String);
/// }
///
/// // Generated: KvInterface, KvGet, KvPut, KvRef, KvClient<P>, KvRequest,
/// // KvServer — all plain manual-API types underneath.
/// use moonpool_rpc::{RpcInterface, RpcMethod};
/// assert_eq!(KvInterface::INTERFACE.get(), 0x6b76_0001);
/// assert_eq!(KvPut::METHOD.get(), 2);
/// # }
/// ```
///
/// Ids are explicit; a duplicate method id is a compile error, also when
/// it hides behind a constant:
///
/// ```compile_fail
/// const GET: u32 = 1;
/// #[moonpool_rpc::service(id = 1, version = 1)]
/// pub trait Kv {
///     #[method(id = GET, schema = 1)]
///     async fn get(&self, key: String) -> String;
///     #[method(id = 1, schema = 1)]
///     async fn put(&self, entry: String);
/// }
/// ```
///
/// See the `moonpool-rpc-derive` crate docs for every generated item.
#[cfg(feature = "derive")]
pub use moonpool_rpc_derive::service;

/// Paths the `derive` feature's generated code uses. Not an API.
#[doc(hidden)]
pub mod __private {
    pub use moonpool_core::Providers;
}
