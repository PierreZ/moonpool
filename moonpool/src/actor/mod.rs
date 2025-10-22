//! Actor lifecycle and core types.

pub mod id;
pub mod lifecycle;
pub mod context;
pub mod catalog;
pub mod traits;
pub mod state;

// Re-exports (only what exists now)
pub use id::{ActorId, CorrelationId, NodeId};
pub use lifecycle::{ActivationState, DeactivationReason};
pub use context::ActorContext;
pub use catalog::{ActorCatalog, ActivationDirectory};
