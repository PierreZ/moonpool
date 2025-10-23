//! Actor lifecycle and core types.

pub mod catalog;
pub mod context;
pub mod extensions;
pub mod factory;
pub mod handlers;
pub mod id;
pub mod lifecycle;
pub mod reference;
pub mod state;
pub mod traits;

// Re-exports (only what exists now)
pub use catalog::{ActivationDirectory, ActorCatalog};
pub use context::{ActorContext, LifecycleCommand, run_message_loop};
pub use factory::ActorFactory;
pub use handlers::HandlerRegistry;
pub use id::{ActorId, CorrelationId, NodeId};
pub use lifecycle::{ActivationState, DeactivationReason};
pub use reference::ActorRef;
pub use state::ActorState;
pub use traits::{Actor, MessageHandler};
