//! Simulated storage file implementation.

use crate::sim::WeakSimWorld;
use crate::sim::state::FileId;
use async_trait::async_trait;
use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};
use moonpool_core::StorageFile;
use std::cell::Cell;
use std::io::{self, SeekFrom};
use std::pin::Pin;
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
/// The file tracks pending operations using `Cell`-based state:
/// - `pending_read`: Active read operation (op_seq, offset, len)
/// - `pending_write`: Active write operation (op_seq, bytes_written)
/// - `seek_state`: Current seek state
///
/// ## Polling Pattern
///
/// Operations follow the schedule → wait → complete pattern:
/// 1. First poll: Schedule operation with SimWorld, store pending state
/// 2. Subsequent polls: Check completion, return Pending until done
/// 3. Final poll: Clear pending state, return result
#[derive(Debug)]
pub struct SimStorageFile {
    sim: WeakSimWorld,
    file_id: FileId,
    /// Pending read operation: (op_seq, offset, len)
    pending_read: Cell<Option<(u64, u64, usize)>>,
    /// Pending write operation: (op_seq, bytes_written)
    pending_write: Cell<Option<(u64, usize)>>,
}

impl SimStorageFile {
    /// Create a new simulated storage file.
    pub(crate) fn new(sim: WeakSimWorld, file_id: FileId) -> Self {
        Self {
            sim,
            file_id,
            pending_read: Cell::new(None),
            pending_write: Cell::new(None),
        }
    }

    /// Get the file ID.
    pub fn file_id(&self) -> FileId {
        self.file_id
    }
}

#[async_trait(?Send)]
impl StorageFile for SimStorageFile {
    async fn sync_all(&self) -> io::Result<()> {
        SyncFuture::new(self.sim.clone(), self.file_id).await
    }

    async fn sync_data(&self) -> io::Result<()> {
        // Simulation treats sync_all and sync_data identically
        SyncFuture::new(self.sim.clone(), self.file_id).await
    }

    async fn size(&self) -> io::Result<u64> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;
        sim.file_size(self.file_id)
            .map_err(|e| io::Error::other(e.to_string()))
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        SetLenFuture::new(self.sim.clone(), self.file_id, size).await
    }
}

impl AsyncRead for SimStorageFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // Check for pending read operation
        if let Some((op_seq, offset, len)) = self.pending_read.get() {
            // Check if operation is complete
            if sim.is_storage_op_complete(self.file_id, op_seq) {
                // Clear pending state
                self.pending_read.set(None);

                // Calculate how many bytes to actually read
                let bytes_to_read = buf.len().min(len);
                if bytes_to_read == 0 {
                    return Poll::Ready(Ok(0));
                }

                // Read from file at the stored offset
                let bytes_read =
                    sim.read_from_file(self.file_id, offset, &mut buf[..bytes_to_read])?;

                // Update file position
                let new_position = offset + bytes_read as u64;
                sim.set_file_position(self.file_id, new_position)?;

                return Poll::Ready(Ok(bytes_read));
            }

            // Operation not complete, register waker and wait
            sim.register_storage_waker(self.file_id, op_seq, cx.waker().clone());
            return Poll::Pending;
        }

        // No pending read - start a new one

        // Get current position
        let position = sim.file_position(self.file_id)?;

        // Get file size to check for EOF
        let file_size = sim.file_size(self.file_id)?;

        // Check for EOF
        if position >= file_size {
            return Poll::Ready(Ok(0)); // EOF - 0 bytes read
        }

        // Calculate bytes to read (don't read past EOF)
        let remaining_in_file = (file_size - position) as usize;
        let len = buf.len().min(remaining_in_file);

        if len == 0 {
            return Poll::Ready(Ok(0));
        }

        // Schedule the read operation
        let op_seq = sim.schedule_read(self.file_id, position, len)?;

        // Store pending state
        self.pending_read.set(Some((op_seq, position, len)));

        // Register waker
        sim.register_storage_waker(self.file_id, op_seq, cx.waker().clone());

        Poll::Pending
    }
}

impl AsyncWrite for SimStorageFile {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // Check for pending write operation
        if let Some((op_seq, bytes_written)) = self.pending_write.get() {
            // Check if operation is complete
            if sim.is_storage_op_complete(self.file_id, op_seq) {
                // Clear pending state
                self.pending_write.set(None);

                // Update file position
                let position = sim.file_position(self.file_id)?;
                let new_position = position + bytes_written as u64;
                sim.set_file_position(self.file_id, new_position)?;

                return Poll::Ready(Ok(bytes_written));
            }

            // Operation not complete, register waker and wait
            sim.register_storage_waker(self.file_id, op_seq, cx.waker().clone());
            return Poll::Pending;
        }

        // No pending write - start a new one

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // Get current position
        let position = sim.file_position(self.file_id)?;

        // Schedule the write operation
        let op_seq = sim.schedule_write(self.file_id, position, buf.to_vec())?;

        // Store pending state
        self.pending_write.set(Some((op_seq, buf.len())));

        // Register waker
        sim.register_storage_waker(self.file_id, op_seq, cx.waker().clone());

        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Flush is a no-op - durability comes from sync_all/sync_data
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Close is a no-op - file cleanup handled via Drop if needed
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for SimStorageFile {
    fn poll_seek(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        let current_position = sim.file_position(self.file_id)?;
        let file_size = sim.file_size(self.file_id)?;

        let target = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::End(offset) => {
                if offset >= 0 {
                    file_size.saturating_add(offset as u64)
                } else {
                    file_size.saturating_sub((-offset) as u64)
                }
            }
            SeekFrom::Current(offset) => {
                if offset >= 0 {
                    current_position.saturating_add(offset as u64)
                } else {
                    current_position.saturating_sub((-offset) as u64)
                }
            }
        };

        sim.set_file_position(self.file_id, target)?;
        Poll::Ready(Ok(target))
    }
}
