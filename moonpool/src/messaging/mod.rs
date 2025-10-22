//! Message routing and correlation.

pub mod message;
pub mod address;
pub mod bus;
pub mod correlation;
pub mod envelope;

// Re-exports
pub use message::{Direction, Message, MessageFlags};
pub use address::ActorAddress;
pub use bus::MessageBus;
pub use correlation::CallbackData;
pub use envelope::ActorEnvelope;

// ActorRef will be defined in the runtime module or here
pub struct ActorRef<A> {
    _phantom: std::marker::PhantomData<A>,
}
