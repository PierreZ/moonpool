//! Deterministic storage simulation engine.

mod complete;
mod engine;
mod event;
mod namespace;
mod namespace_ops;
mod shards;
mod state;

pub use engine::StorageEngine;
pub(crate) use engine::{StorageActions, StorageCompletion};
pub use event::{OperationId, StorageEvent};
pub(crate) use state::NamespaceOp;
pub use state::{DiskDegradationState, DiskEpisodeKind, FileId, HandleId};
