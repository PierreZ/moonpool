//! Common imports for moonpool actor system.
//!
//! This module provides a convenient prelude for importing commonly used types and traits.

// Re-export core types (only what exists now)
pub use crate::actor::{ActorId, CorrelationId, NodeId, ActivationState, DeactivationReason};
pub use crate::error::{ActorError, DirectoryError, MessageError, StorageError};
pub use crate::messaging::{ActorAddress, Direction, Message, MessageFlags};

// Re-export commonly used external types
pub use async_trait::async_trait;
pub use serde::{Deserialize, Serialize};
pub use std::sync::Arc;
pub use std::time::Duration;

// Re-export Result type for convenience
pub type Result<T> = std::result::Result<T, ActorError>;
