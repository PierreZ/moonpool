//! Events handled by the simulated storage engine.

use super::HandleId;
use crate::storage::StorageOperation;

/// Stable identifier for one simulated storage operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId(pub(crate) u64);

/// A completion targeted at one exact pending storage operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageEvent {
    operation_id: OperationId,
    handle_id: HandleId,
    operation: StorageOperation,
}

impl StorageEvent {
    pub(crate) fn new(
        operation_id: OperationId,
        handle_id: HandleId,
        operation: StorageOperation,
    ) -> Self {
        Self {
            operation_id,
            handle_id,
            operation,
        }
    }

    /// Returns the operation completed by this event.
    #[must_use]
    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Returns the handle that submitted the operation.
    #[must_use]
    pub fn handle_id(&self) -> HandleId {
        self.handle_id
    }

    /// Returns the operation kind carried for diagnostics.
    #[must_use]
    pub fn operation(&self) -> StorageOperation {
        self.operation
    }
}
