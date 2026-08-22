//! Future types for storage async operations.
//!
//! These futures handle the schedule → wait → complete pattern for
//! storage operations that don't fit into the standard AsyncRead/AsyncWrite
//! traits.

use crate::sim::WeakSimWorld;
use crate::storage::sim::{HandleId, OperationId, StorageCompletion};
use std::cell::Cell;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::sim_shutdown_error;

/// Future for `sync_all` and `sync_data` operations.
///
/// Follows the schedule → wait → complete pattern:
/// 1. First poll: Schedule sync with `SimWorld`, store `op_seq`
/// 2. Subsequent polls: Check completion, return Pending until done
/// 3. Final poll: Clear state, return Ok(())
pub struct SyncFuture {
    sim: WeakSimWorld,
    handle_id: HandleId,
    /// Pending operation sequence number
    pending_op: Cell<Option<OperationId>>,
}

impl SyncFuture {
    /// Create a new sync future.
    pub(crate) fn new(sim: WeakSimWorld, handle_id: HandleId) -> Self {
        Self {
            sim,
            handle_id,
            pending_op: Cell::new(None),
        }
    }
}

impl Future for SyncFuture {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // Check for pending operation
        if let Some(operation_id) = self.pending_op.get() {
            // Check if operation is complete
            if let Poll::Ready(result) = sim.poll_storage_operation(operation_id, cx.waker()) {
                // Clear pending state
                self.pending_op.set(None);
                return Poll::Ready(match result {
                    Ok(StorageCompletion::Unit) => Ok(()),
                    Ok(_) => Err(io::Error::other(
                        "sync operation returned a value completion",
                    )),
                    Err(error) => Err(error.into()),
                });
            }
            return Poll::Pending;
        }

        // No pending operation - start a new one
        let operation_id = sim.schedule_sync(self.handle_id)?;

        // Store pending state
        self.pending_op.set(Some(operation_id));

        // Register waker
        let _ = sim.poll_storage_operation(operation_id, cx.waker());

        Poll::Pending
    }
}

impl Drop for SyncFuture {
    fn drop(&mut self) {
        if let Some(operation_id) = self.pending_op.get()
            && let Ok(sim) = self.sim.upgrade()
        {
            sim.cancel_storage_operation(operation_id);
        }
    }
}

/// Future for `set_len` operations.
///
/// Follows the schedule → wait → complete pattern:
/// 1. First poll: Schedule `set_len` with `SimWorld`, store `op_seq`
/// 2. Subsequent polls: Check completion, return Pending until done
/// 3. Final poll: Clear state, return Ok(())
pub struct SetLenFuture {
    sim: WeakSimWorld,
    handle_id: HandleId,
    /// The new length to set the file to.
    new_len: u64,
    /// Pending operation sequence number
    pending_op: Cell<Option<OperationId>>,
}

impl SetLenFuture {
    /// Create a new `set_len` future.
    pub(crate) fn new(sim: WeakSimWorld, handle_id: HandleId, new_len: u64) -> Self {
        Self {
            sim,
            handle_id,
            new_len,
            pending_op: Cell::new(None),
        }
    }
}

impl Future for SetLenFuture {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // Check for pending operation
        if let Some(operation_id) = self.pending_op.get() {
            // Check if operation is complete
            if let Poll::Ready(result) = sim.poll_storage_operation(operation_id, cx.waker()) {
                // Clear pending state
                self.pending_op.set(None);
                return Poll::Ready(match result {
                    Ok(StorageCompletion::Unit) => Ok(()),
                    Ok(_) => Err(io::Error::other(
                        "set_len operation returned a value completion",
                    )),
                    Err(error) => Err(error.into()),
                });
            }
            return Poll::Pending;
        }

        // No pending operation - start a new one
        let operation_id = sim.schedule_set_len(self.handle_id, self.new_len)?;

        // Store pending state
        self.pending_op.set(Some(operation_id));

        // Register waker
        let _ = sim.poll_storage_operation(operation_id, cx.waker());

        Poll::Pending
    }
}

impl Drop for SetLenFuture {
    fn drop(&mut self) {
        if let Some(operation_id) = self.pending_op.get()
            && let Ok(sim) = self.sim.upgrade()
        {
            sim.cancel_storage_operation(operation_id);
        }
    }
}
