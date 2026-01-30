//! Future types for storage async operations.
//!
//! These futures handle the schedule → wait → complete pattern for
//! storage operations that don't fit into the standard AsyncRead/AsyncWrite
//! traits.

use crate::sim::WeakSimWorld;
use crate::sim::state::FileId;
use std::cell::Cell;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Create an io::Error for simulation shutdown.
fn sim_shutdown_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "simulation shutdown")
}

/// Future for sync_all and sync_data operations.
///
/// Follows the schedule → wait → complete pattern:
/// 1. First poll: Schedule sync with SimWorld, store op_seq
/// 2. Subsequent polls: Check completion, return Pending until done
/// 3. Final poll: Clear state, return Ok(())
pub struct SyncFuture {
    sim: WeakSimWorld,
    file_id: FileId,
    /// Pending operation sequence number
    pending_op: Cell<Option<u64>>,
}

impl SyncFuture {
    /// Create a new sync future.
    pub(crate) fn new(sim: WeakSimWorld, file_id: FileId) -> Self {
        Self {
            sim,
            file_id,
            pending_op: Cell::new(None),
        }
    }
}

impl Future for SyncFuture {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // Check for pending operation
        if let Some(op_seq) = self.pending_op.get() {
            // Check if operation is complete
            if sim.is_storage_op_complete(self.file_id, op_seq) {
                // Clear pending state
                self.pending_op.set(None);
                return Poll::Ready(Ok(()));
            }

            // Operation not complete, register waker and wait
            sim.register_storage_waker(self.file_id, op_seq, cx.waker().clone());
            return Poll::Pending;
        }

        // No pending operation - start a new one
        let op_seq = sim
            .schedule_sync(self.file_id)
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Store pending state
        self.pending_op.set(Some(op_seq));

        // Register waker
        sim.register_storage_waker(self.file_id, op_seq, cx.waker().clone());

        Poll::Pending
    }
}

/// Future for set_len operations.
///
/// Follows the schedule → wait → complete pattern:
/// 1. First poll: Schedule set_len with SimWorld, store op_seq
/// 2. Subsequent polls: Check completion, return Pending until done
/// 3. Final poll: Clear state, return Ok(())
pub struct SetLenFuture {
    sim: WeakSimWorld,
    file_id: FileId,
    new_len: u64,
    /// Pending operation sequence number
    pending_op: Cell<Option<u64>>,
}

impl SetLenFuture {
    /// Create a new set_len future.
    pub(crate) fn new(sim: WeakSimWorld, file_id: FileId, new_len: u64) -> Self {
        Self {
            sim,
            file_id,
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
        if let Some(op_seq) = self.pending_op.get() {
            // Check if operation is complete
            if sim.is_storage_op_complete(self.file_id, op_seq) {
                // Clear pending state
                self.pending_op.set(None);
                return Poll::Ready(Ok(()));
            }

            // Operation not complete, register waker and wait
            sim.register_storage_waker(self.file_id, op_seq, cx.waker().clone());
            return Poll::Pending;
        }

        // No pending operation - start a new one
        let op_seq = sim
            .schedule_set_len(self.file_id, self.new_len)
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Store pending state
        self.pending_op.set(Some(op_seq));

        // Register waker
        sim.register_storage_waker(self.file_id, op_seq, cx.waker().clone());

        Poll::Pending
    }
}
