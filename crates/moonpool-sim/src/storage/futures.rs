//! Future types for storage async operations.
//!
//! These futures handle the schedule → wait → complete pattern for
//! storage operations that don't fit into the standard AsyncRead/AsyncWrite
//! traits.

use crate::sim::{SimWorld, WeakSimWorld};
use crate::storage::sim::{HandleId, OperationId, StorageCompletion};
use std::cell::Cell;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::sim_shutdown_error;

/// The schedule → wait → complete state every storage future shares.
///
/// 1. First poll: schedule the operation with `SimWorld`, store its id
/// 2. Subsequent polls: check completion, return Pending until done
/// 3. Final poll: clear state, map the completion to the future's output
///
/// Dropping it while an operation is pending cancels that operation.
struct PendingOperation {
    sim: WeakSimWorld,
    pending_op: Cell<Option<OperationId>>,
}

impl PendingOperation {
    fn new(sim: WeakSimWorld) -> Self {
        Self {
            sim,
            pending_op: Cell::new(None),
        }
    }

    /// Drive one poll: `schedule` starts the operation on the first poll,
    /// `complete` maps its completion (`None` means the engine answered with
    /// the wrong kind of completion, reported as `mismatch`).
    fn poll<T>(
        &self,
        cx: &Context<'_>,
        schedule: impl FnOnce(&SimWorld) -> io::Result<OperationId>,
        complete: impl FnOnce(StorageCompletion) -> Option<T>,
        mismatch: &'static str,
    ) -> Poll<io::Result<T>> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        if let Some(operation_id) = self.pending_op.get() {
            if let Poll::Ready(result) = sim.poll_storage_operation(operation_id, cx.waker()) {
                self.pending_op.set(None);
                return Poll::Ready(match result {
                    Ok(completion) => {
                        complete(completion).ok_or_else(|| io::Error::other(mismatch))
                    }
                    Err(error) => Err(error.into()),
                });
            }
            return Poll::Pending;
        }

        let operation_id = schedule(&sim)?;
        self.pending_op.set(Some(operation_id));
        let _ = sim.poll_storage_operation(operation_id, cx.waker());
        Poll::Pending
    }
}

impl Drop for PendingOperation {
    fn drop(&mut self) {
        if let Some(operation_id) = self.pending_op.get()
            && let Ok(sim) = self.sim.upgrade()
        {
            sim.cancel_storage_operation(operation_id);
        }
    }
}

/// Future for `sync_all` and `sync_data` operations.
pub struct SyncFuture {
    op: PendingOperation,
    handle_id: HandleId,
}

impl SyncFuture {
    /// Create a new sync future.
    pub(crate) fn new(sim: WeakSimWorld, handle_id: HandleId) -> Self {
        Self {
            op: PendingOperation::new(sim),
            handle_id,
        }
    }
}

impl Future for SyncFuture {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.op.poll(
            cx,
            |sim| Ok(sim.schedule_sync(self.handle_id)?),
            |completion| matches!(completion, StorageCompletion::Unit).then_some(()),
            "sync operation returned a value completion",
        )
    }
}

/// Future for `set_len` operations.
pub struct SetLenFuture {
    op: PendingOperation,
    handle_id: HandleId,
    /// The new length to set the file to.
    new_len: u64,
}

impl SetLenFuture {
    /// Create a new `set_len` future.
    pub(crate) fn new(sim: WeakSimWorld, handle_id: HandleId, new_len: u64) -> Self {
        Self {
            op: PendingOperation::new(sim),
            handle_id,
            new_len,
        }
    }
}

impl Future for SetLenFuture {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.op.poll(
            cx,
            |sim| Ok(sim.schedule_set_len(self.handle_id, self.new_len)?),
            |completion| matches!(completion, StorageCompletion::Unit).then_some(()),
            "set_len operation returned a value completion",
        )
    }
}

/// Future for one positioned read (`read_at`).
///
/// Same schedule → wait → complete pattern as [`SyncFuture`], but the
/// completion carries bytes and the handle's stream cursor is never touched.
pub struct ReadAtFuture {
    op: PendingOperation,
    handle_id: HandleId,
    offset: u64,
    len: usize,
}

impl ReadAtFuture {
    pub(crate) fn new(sim: WeakSimWorld, handle_id: HandleId, offset: u64, len: usize) -> Self {
        Self {
            op: PendingOperation::new(sim),
            handle_id,
            offset,
            len,
        }
    }
}

impl Future for ReadAtFuture {
    type Output = io::Result<Vec<u8>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.op.poll(
            cx,
            |sim| Ok(sim.schedule_positioned_read(self.handle_id, self.offset, self.len)?),
            |completion| match completion {
                StorageCompletion::Read(data) => Some(data),
                _ => None,
            },
            "read operation returned a non-read completion",
        )
    }
}

/// Future for one positioned write (`write_at`).
pub struct WriteAtFuture {
    op: PendingOperation,
    handle_id: HandleId,
    offset: u64,
    /// Taken on the first poll, when the operation is scheduled.
    data: Cell<Option<Vec<u8>>>,
}

impl WriteAtFuture {
    pub(crate) fn new(sim: WeakSimWorld, handle_id: HandleId, offset: u64, data: Vec<u8>) -> Self {
        Self {
            op: PendingOperation::new(sim),
            handle_id,
            offset,
            data: Cell::new(Some(data)),
        }
    }
}

impl Future for WriteAtFuture {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.op.poll(
            cx,
            |sim| {
                let Some(data) = self.data.take() else {
                    return Err(io::Error::other("positioned write polled after completion"));
                };
                Ok(sim.schedule_positioned_write(self.handle_id, self.offset, data)?)
            },
            |completion| match completion {
                StorageCompletion::Write { len, .. } => Some(len),
                _ => None,
            },
            "write operation returned a non-write completion",
        )
    }
}
