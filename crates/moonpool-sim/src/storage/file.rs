//! Simulated storage file implementation.

use crate::sim::WeakSimWorld;
use crate::storage::sim::{HandleId, OperationId, StorageCompletion};
use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};
use moonpool_core::StorageFile;
use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use super::futures::{SetLenFuture, SyncFuture};
use super::sim_shutdown_error;

/// Simulated storage file for deterministic testing.
///
/// This provides a simulation-aware file handle that integrates with
/// the deterministic simulation engine for testing storage I/O patterns.
///
/// ## State Tracking
///
/// The file tracks pending operations directly on the handle:
/// - `pending_read`: Active read operation (`op_seq`, offset, len)
/// - `pending_write`: Active write operation (`op_seq`, `bytes_written`)
///
/// ## Polling Pattern
///
/// Operations follow the schedule → wait → complete pattern:
/// 1. First poll: Schedule operation with `SimWorld`, store pending state
/// 2. Subsequent polls: Check completion, return Pending until done
/// 3. Final poll: Clear pending state, return result
#[derive(Debug)]
pub struct SimStorageFile {
    sim: WeakSimWorld,
    handle_id: HandleId,
    closed: AtomicBool,
    /// Pending read operation: (`op_seq`, offset, len)
    pending_read: Option<(OperationId, u64, usize)>,
    /// Pending write operation: (`op_seq`, `bytes_written`)
    pending_write: Option<(OperationId, usize)>,
}

impl SimStorageFile {
    /// Create a new simulated storage file.
    pub(crate) fn new(sim: WeakSimWorld, handle_id: HandleId) -> Self {
        Self {
            sim,
            handle_id,
            closed: AtomicBool::new(false),
            pending_read: None,
            pending_write: None,
        }
    }

    fn ensure_open(&self) -> io::Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "file is closed"))
        } else {
            Ok(())
        }
    }
}

impl StorageFile for SimStorageFile {
    async fn sync_all(&self) -> io::Result<()> {
        self.ensure_open()?;
        SyncFuture::new(self.sim.clone(), self.handle_id).await
    }

    async fn sync_data(&self) -> io::Result<()> {
        // Simulation treats sync_all and sync_data identically
        self.ensure_open()?;
        SyncFuture::new(self.sim.clone(), self.handle_id).await
    }

    async fn size(&self) -> io::Result<u64> {
        self.ensure_open()?;
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;
        sim.file_size(self.handle_id).map_err(Into::into)
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        self.ensure_open()?;
        SetLenFuture::new(self.sim.clone(), self.handle_id, size).await
    }
}

impl AsyncRead for SimStorageFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.ensure_open()?;
        let sim = this.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // Check for pending read operation
        if let Some((operation_id, offset, len)) = this.pending_read {
            // Check if operation is complete
            if let Poll::Ready(result) = sim.poll_storage_operation(operation_id, cx.waker()) {
                this.pending_read = None;
                let StorageCompletion::Read(data) = result? else {
                    return Poll::Ready(Err(io::Error::other(
                        "read operation returned a non-read completion",
                    )));
                };
                let bytes_read = buf.len().min(data.len()).min(len);
                buf[..bytes_read].copy_from_slice(&data[..bytes_read]);

                // Update file position
                let new_position = offset + bytes_read as u64;
                sim.set_file_position(this.handle_id, new_position)?;

                return Poll::Ready(Ok(bytes_read));
            }

            // Operation not complete, register waker and wait
            return Poll::Pending;
        }

        // No pending read - start a new one

        // Get current position
        let position = sim.file_position(this.handle_id)?;

        // Get file size to check for EOF
        let file_size = sim.file_size(this.handle_id)?;

        // Check for EOF
        if position >= file_size {
            return Poll::Ready(Ok(0)); // EOF - 0 bytes read
        }

        // Calculate bytes to read (don't read past EOF)
        let remaining_in_file =
            usize::try_from(file_size - position).expect("remaining bytes in file fit in usize");
        let len = buf.len().min(remaining_in_file);

        if len == 0 {
            return Poll::Ready(Ok(0));
        }

        // Schedule the read operation
        let operation_id = sim.schedule_read(this.handle_id, position, len)?;

        // Store pending state
        this.pending_read = Some((operation_id, position, len));

        // Register waker
        let _ = sim.poll_storage_operation(operation_id, cx.waker());

        Poll::Pending
    }
}

impl AsyncWrite for SimStorageFile {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.ensure_open()?;
        let sim = this.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // Check for pending write operation
        if let Some((operation_id, bytes_written)) = this.pending_write {
            // Check if operation is complete
            if let Poll::Ready(result) = sim.poll_storage_operation(operation_id, cx.waker()) {
                this.pending_write = None;
                let StorageCompletion::Write { offset, len } = result? else {
                    return Poll::Ready(Err(io::Error::other(
                        "write operation returned a non-write completion",
                    )));
                };
                debug_assert_eq!(len, bytes_written);
                tracing::trace!(offset, len, "storage write completed");
                return Poll::Ready(Ok(len));
            }

            // Operation not complete, register waker and wait
            return Poll::Pending;
        }

        // No pending write - start a new one

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // Get current position
        let position = sim.file_position(this.handle_id)?;

        // Schedule the write operation
        let operation_id = sim.schedule_write(this.handle_id, position, buf.to_vec())?;

        // Store pending state
        this.pending_write = Some((operation_id, buf.len()));

        // Register waker
        let _ = sim.poll_storage_operation(operation_id, cx.waker());

        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Flush is a no-op - durability comes from sync_all/sync_data
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.closed.swap(true, Ordering::Relaxed)
            && let Ok(sim) = this.sim.upgrade()
        {
            if let Some((operation_id, ..)) = this.pending_read {
                sim.cancel_storage_operation(operation_id);
            }
            if let Some((operation_id, ..)) = this.pending_write {
                sim.cancel_storage_operation(operation_id);
            }
            sim.close_storage_handle(this.handle_id);
        }
        this.pending_read = None;
        this.pending_write = None;
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for SimStorageFile {
    fn poll_seek(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        self.ensure_open()?;
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        let current_position = sim.file_position(self.handle_id)?;
        let file_size = sim.file_size(self.handle_id)?;

        let target = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::End(offset) => {
                if offset >= 0 {
                    file_size.saturating_add(offset.unsigned_abs())
                } else {
                    file_size.saturating_sub(offset.unsigned_abs())
                }
            }
            SeekFrom::Current(offset) => {
                if offset >= 0 {
                    current_position.saturating_add(offset.unsigned_abs())
                } else {
                    current_position.saturating_sub(offset.unsigned_abs())
                }
            }
        };

        sim.set_file_position(self.handle_id, target)?;
        Poll::Ready(Ok(target))
    }
}

impl Drop for SimStorageFile {
    fn drop(&mut self) {
        if !self.closed.swap(true, Ordering::Relaxed)
            && let Ok(sim) = self.sim.upgrade()
        {
            if let Some((operation_id, ..)) = self.pending_read {
                sim.cancel_storage_operation(operation_id);
            }
            if let Some((operation_id, ..)) = self.pending_write {
                sim.cancel_storage_operation(operation_id);
            }
            sim.close_storage_handle(self.handle_id);
        }
    }
}
