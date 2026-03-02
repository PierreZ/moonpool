//! Actor runtime: host-side processing loops and caller-side routing.

pub(crate) mod host;
pub(crate) mod router;

pub use host::{ActorContext, ActorHandler, ActorHost, DeactivationHint};
pub use router::{ActorError, ActorRouter};
