//! Message routing and correlation.

pub mod activation;
pub mod address;
pub mod bus;
pub mod correlation;
pub mod envelope;
pub mod message;
pub mod network;
pub mod router;

// Re-exports
pub use activation::{ActivationRequest, ActivationResponse, ActivationResult};
pub use address::ActorAddress;
pub use bus::MessageBus;
pub use correlation::{CallbackConfig, CallbackData, CallbackResponse, CorrelationIdFactory};
pub use envelope::ActorEnvelope;
pub use message::{Direction, Message, MessageFlags};
pub use network::{FoundationTransport, NetworkTransport};
pub use router::ActorRouter;
