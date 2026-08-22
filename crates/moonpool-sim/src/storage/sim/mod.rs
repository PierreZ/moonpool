//! Deterministic storage simulation engine.

mod engine;
mod event;
mod state;

pub use engine::StorageEngine;
pub(crate) use engine::{StorageActions, StorageCompletion};
pub use event::{OperationId, StorageEvent};
pub use state::{DiskDegradationState, DiskEpisodeKind, FileId, HandleId};
