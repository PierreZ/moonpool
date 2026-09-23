//! Interfaces as data: serialisable references, endpoint groups and their
//! checked method adjustment.
//!
//! A service **interface** is a set of methods served by one dynamic
//! endpoint group: one registry slot, one runtime incarnation, one
//! [`AccessClass`](crate::AccessClass), several [`RpcMethod`]s told apart
//! by their explicit [`MethodId`](crate::MethodId)s (`FoundationDB`'s
//! `getAdjustedEndpoint`, with stable method identifiers instead of
//! declaration offsets).
//!
//! - [`RpcInterface`] names an interface with an explicit [`InterfaceId`]
//!   and version; [`InterfaceMethod`] declares a method a member.
//! - [`RpcHandle::register_group`](crate::RpcHandle::register_group)
//!   allocates a [`ServiceGroup`]; [`ServiceGroup::serve`] opens one
//!   method's [`RequestStream`](crate::RequestStream).
//! - [`InterfaceRef`] and [`ServiceRef`] are immutable routing data and
//!   protobuf messages: return them in replies, embed them in requests,
//!   persist them, forward them to a third participant. Decoding one
//!   needs no runtime and keeps nothing alive.
//! - Binding ([`InterfaceRef::bind`], [`ServiceRef::bind`]) picks the
//!   runtime that carries the calls; it registers nothing.
//!
//! # Lifetimes
//!
//! Dropping the [`ServiceGroup`] destroys the group: new requests for any of
//! its methods fail with [`EndpointNotFound`](crate::ErrorReason::EndpointNotFound),
//! requests queued but not yet received complete as broken promises, and
//! every stream of the group ends. A request already received finishes
//! under its own [`ReplyHandle`](crate::ReplyHandle), which is the owned
//! guard of admitted work: replying after the group is gone still reaches
//! the caller over the session it came from. Dropping one method's stream
//! removes only that method; the group keeps serving the others and the
//! method may be served again under the same reference.
//!
//! # Fresh incarnations are explicit
//!
//! A reference names one runtime incarnation. After a restart at the same
//! address every reference to the old incarnation is refused
//! ([`StaleIncarnation`](crate::ErrorReason::StaleIncarnation)) and never
//! dispatched to the new one; the new incarnation publishes new references
//! and callers learn them through the application (a well-known directory,
//! a recruitment reply). Nothing refreshes a dynamic reference by itself.
//!
//! # Callbacks
//!
//! A [`ReplyHandle`](crate::ReplyHandle) is bound to the session that
//! delivered its request and has no byte form: it cannot be forwarded. A
//! callback that a third party may invoke is an ordinary dynamic endpoint:
//! register it, and put its [`ServiceRef`] in the request.

mod group;
pub(crate) mod proto;
mod reference;

pub use group::ServiceGroup;
pub use reference::{InterfaceClient, InterfaceRef, ServiceRef};

use crate::protocol::{RpcMethod, SchemaVersion};

/// Stable 32-bit identifier of a service interface.
///
/// Application-chosen and written down as a constant, like
/// [`MethodId`](crate::MethodId); never derived from a Rust name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InterfaceId(u32);

impl InterfaceId {
    /// Wrap an application-chosen identifier.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The raw identifier.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for InterfaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "interface:{:#010x}", self.0)
    }
}

/// A service interface: the identity of an endpoint group.
///
/// ```
/// use moonpool_rpc::{InterfaceId, RpcInterface, SchemaVersion};
///
/// /// A key-value store.
/// pub struct Kv;
///
/// impl RpcInterface for Kv {
///     const INTERFACE: InterfaceId = InterfaceId::new(0x6b76_0001);
///     const VERSION: SchemaVersion = SchemaVersion::new(1);
///     const NAME: &'static str = "kv";
/// }
/// ```
pub trait RpcInterface: Send + Sync + 'static {
    /// The stable identity of the interface.
    const INTERFACE: InterfaceId;
    /// The version of the interface as a whole; a reference to another
    /// version is refused by [`InterfaceRef::check`]. Evolve methods
    /// through their own [`SchemaVersion`]s, and bump this only when the
    /// set of methods changes incompatibly.
    const VERSION: SchemaVersion;
    /// A human-readable name for traces and errors. Never sent.
    const NAME: &'static str;
}

/// Declares `Self` a method of interface `I`.
///
/// Membership is what [`InterfaceRef::method`] and [`ServiceGroup::serve`]
/// check at compile time; the method's own [`MethodId`](crate::MethodId) is
/// what the server checks at admission.
pub trait InterfaceMethod<I: RpcInterface>: RpcMethod {}
