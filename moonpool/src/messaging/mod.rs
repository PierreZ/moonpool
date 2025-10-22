//! Message routing and correlation.

pub mod address;
pub mod bus;
pub mod correlation;
pub mod envelope;
pub mod message;

// Re-exports
pub use address::ActorAddress;
pub use bus::MessageBus;
pub use correlation::{CallbackConfig, CallbackData, CallbackResponse, CorrelationIdFactory};
pub use envelope::ActorEnvelope;
pub use message::{Direction, Message, MessageFlags};

// ActorRef will be defined in the runtime module or here
pub struct ActorRef<A> {
    _phantom: std::marker::PhantomData<A>,
}
