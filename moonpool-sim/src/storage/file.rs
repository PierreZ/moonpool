//! Simulated storage file implementation.

use crate::sim::WeakSimWorld;
use crate::sim::state::FileId;
use async_trait::async_trait;
use moonpool_core::StorageFile;
use std::cell::Cell;
use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};

use super::futures::{SetLenFuture, SyncFuture};
use super::sim_shutdown_error;

/// State for an in-progress seek operation.
#[derive(Debug, Clone, Copy)]
enum SeekState {
    /// No seek in progress.
    Idle,
    /// Seek requested, waiting to complete.
    Seeking(u64),
}

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
    /// Current seek state
    seek_state: Cell<SeekState>,
}

impl SimStorageFile {
    /// Create a new simulated storage file.
    pub(crate) fn new(sim: WeakSimWorld, file_id: FileId) -> Self {
        Self {
            sim,
            file_id,
            pending_read: Cell::new(None),
            pending_write: Cell::new(None),
            seek_state: Cell::new(SeekState::Idle),
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
        sim.get_file_size(self.file_id)
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
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // Check for pending read operation
        if let Some((op_seq, offset, len)) = self.pending_read.get() {
            // Check if operation is complete
            if sim.is_storage_op_complete(self.file_id, op_seq) {
                // Clear pending state
                self.pending_read.set(None);

                // Calculate how many bytes to actually read
                let bytes_to_read = buf.remaining().min(len);
                if bytes_to_read == 0 {
                    return Poll::Ready(Ok(()));
                }

                // Read from file at the stored offset
                let mut temp_buf = vec![0u8; bytes_to_read];
                let bytes_read = sim
                    .read_from_file(self.file_id, offset, &mut temp_buf)
                    .map_err(|e| io::Error::other(e.to_string()))?;

                // Update file position
                let new_position = offset + bytes_read as u64;
                sim.set_file_position(self.file_id, new_position)
                    .map_err(|e| io::Error::other(e.to_string()))?;

                // Copy to output buffer
                buf.put_slice(&temp_buf[..bytes_read]);
                return Poll::Ready(Ok(()));
            }

            // Operation not complete, register waker and wait
            sim.register_storage_waker(self.file_id, op_seq, cx.waker().clone());
            return Poll::Pending;
        }

        // No pending read - start a new one

        // Get current position
        let position = sim
            .get_file_position(self.file_id)
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Get file size to check for EOF
        let file_size = sim
            .get_file_size(self.file_id)
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Check for EOF
        if position >= file_size {
            return Poll::Ready(Ok(())); // EOF - 0 bytes read
        }

        // Calculate bytes to read (don't read past EOF)
        let remaining_in_file = (file_size - position) as usize;
        let len = buf.remaining().min(remaining_in_file);

        if len == 0 {
            return Poll::Ready(Ok(()));
        }

        // Schedule the read operation
        let op_seq = sim
            .schedule_read(self.file_id, position, len)
            .map_err(|e| io::Error::other(e.to_string()))?;

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
                let position = sim
                    .get_file_position(self.file_id)
                    .map_err(|e| io::Error::other(e.to_string()))?;
                let new_position = position + bytes_written as u64;
                sim.set_file_position(self.file_id, new_position)
                    .map_err(|e| io::Error::other(e.to_string()))?;

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
        let position = sim
            .get_file_position(self.file_id)
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Schedule the write operation
        let op_seq = sim
            .schedule_write(self.file_id, position, buf.to_vec())
            .map_err(|e| io::Error::other(e.to_string()))?;

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

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Shutdown is a no-op - file cleanup handled via Drop if needed
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for SimStorageFile {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // Calculate target position
        let current_position = sim
            .get_file_position(self.file_id)
            .map_err(|e| io::Error::other(e.to_string()))?;

        let file_size = sim
            .get_file_size(self.file_id)
            .map_err(|e| io::Error::other(e.to_string()))?;

        let target = match position {
            SeekFrom::Start(pos) => pos,
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

        // Store the seek state
        self.seek_state.set(SeekState::Seeking(target));
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        match self.seek_state.get() {
            SeekState::Idle => {
                // No seek in progress, return current position
                let position = sim
                    .get_file_position(self.file_id)
                    .map_err(|e| io::Error::other(e.to_string()))?;
                Poll::Ready(Ok(position))
            }
            SeekState::Seeking(target) => {
                // Complete the seek by setting the position
                sim.set_file_position(self.file_id, target)
                    .map_err(|e| io::Error::other(e.to_string()))?;
                self.seek_state.set(SeekState::Idle);
                Poll::Ready(Ok(target))
            }
        }
    }
}
