//! Typed actor reference construction trait.
//!
//! [`ActorRef`] enables the `node.actor_ref::<MyRef<_>>("id")` pattern,
//! implemented by the `#[service]` macro for virtual actor types.

use std::rc::Rc;

use crate::{JsonCodec, MessageCodec, Providers};

use super::runtime::router::ActorRouter;

/// Trait for constructing typed actor references from a router.
///
/// Implemented by the `#[service]` macro for virtual actor types.
/// Enables `node.actor_ref::<MyRef<_>>("id")` syntax.
///
/// The codec parameter defaults to [`JsonCodec`] for external usage
/// (e.g. `node.actor_ref()`), but can be any codec when used inside
/// actors via `ctx.actor_ref()`.
pub trait ActorRef<P: Providers, C: MessageCodec = JsonCodec>: Sized {
    /// Create a reference to the actor with the given identity.
    fn from_router(identity: impl Into<String>, router: &Rc<ActorRouter<P, C>>) -> Self;
}
