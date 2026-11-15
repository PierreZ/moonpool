//! Simulated storage file implementation.

use crate::sim::WeakSimWorld;
use crate::storage::sim::HandleId;
use moonpool_core::{IoConstraints, StorageFile};
use std::io;

use super::futures::{AllocateFuture, ReadAtFuture, SetLenFuture, SyncFuture, WriteAtFuture};
use super::sim_shutdown_error;

/// How often an active durable-write point fires per call: the window a
/// two-call write-then-sync once named itself, at the rate of the journal
/// commit points it replaces.
const DURABLE_WRITE_HINT_PROB: f64 = 0.25;

/// Simulated storage file for deterministic testing.
///
/// This provides a simulation-aware file handle that integrates with
/// the deterministic simulation engine for testing storage I/O patterns.
///
/// Every operation is a future that schedules a storage event on its first
/// poll and completes when the world delivers it.
#[derive(Debug)]
pub struct SimStorageFile {
    sim: WeakSimWorld,
    handle_id: HandleId,
    /// Alignment every transfer through this handle must satisfy, fixed when
    /// the file was opened. `IoConstraints::NONE` for a buffered open.
    constraints: IoConstraints,
    /// Whether the open actually got direct (uncached) I/O.
    direct_io: bool,
}

impl SimStorageFile {
    /// Create a new simulated storage file.
    pub(crate) fn new(
        sim: WeakSimWorld,
        handle_id: HandleId,
        constraints: IoConstraints,
        direct_io: bool,
    ) -> Self {
        Self {
            sim,
            handle_id,
            constraints,
            direct_io,
        }
    }
}

impl StorageFile for SimStorageFile {
    async fn sync_all(&self) -> io::Result<()> {
        SyncFuture::new(self.sim.clone(), self.handle_id).await
    }

    async fn sync_data(&self) -> io::Result<()> {
        // Simulation treats sync_all and sync_data identically
        self.sync_all().await
    }

    fn constraints(&self) -> IoConstraints {
        self.constraints
    }

    fn is_direct_io(&self) -> bool {
        self.direct_io
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.constraints.check(offset, buf)?;
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;
        let size = sim.file_size(self.handle_id)?;
        if offset >= size {
            return Ok(0);
        }
        let remaining = usize::try_from(size - offset).unwrap_or(usize::MAX);
        let len = buf.len().min(remaining);
        let data = ReadAtFuture::new(self.sim.clone(), self.handle_id, offset, len).await?;
        let read = data.len().min(buf.len());
        buf[..read].copy_from_slice(&data[..read]);
        Ok(read)
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.constraints.check(offset, buf)?;
        WriteAtFuture::new(self.sim.clone(), self.handle_id, offset, buf.to_vec()).await
    }

    /// Modelled as [`write_at`](Self::write_at), then a sync of that range
    /// only, as `RWF_DSYNC` does on Linux. The write lands as a dirty range
    /// and the range sync commits it, so the barrier-bounded crash model, the
    /// lost-synced-write oracle, sync failures and lying syncs all apply to a
    /// durable write unchanged, as they do in `FoundationDB`'s
    /// `AsyncFileNonDurable`, where only `sync()` makes a write durable.
    ///
    /// The crash window inside the call is real on a device too (a power cut
    /// mid-FUA leaves the range old, new or torn), so it is a named moment:
    /// the process may die with the bytes written and not yet synced, and
    /// the crash model resolves them.
    ///
    /// An earlier unsynced write outside the range stays volatile: a caller
    /// that relies on a durable write to cover it loses it in a crash, as on
    /// Linux.
    async fn write_at_durable(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        let written = self.write_at(offset, buf).await?;
        if written == 0 {
            return Ok(0);
        }
        moonpool_buggify::hint!(
            "storage: durable write landed, not synced",
            DURABLE_WRITE_HINT_PROB
        )
        .await;
        SyncFuture::range(self.sim.clone(), self.handle_id, offset, written).await?;
        Ok(written)
    }

    async fn size(&self) -> io::Result<u64> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;
        sim.file_size(self.handle_id).map_err(Into::into)
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        SetLenFuture::new(self.sim.clone(), self.handle_id, size).await
    }

    /// Reserves disk space for `[0, len)`, as `fallocate` does: the file
    /// grows to `len` when it is shorter, the new range reads as zeros, and
    /// a later write inside the range never fails for space. `ENOSPC` when
    /// the space does not fit; then nothing changes. A `set_len` growth, by
    /// contrast, is a hole that reserves nothing. Like `set_len` it is a
    /// metadata write: it needs a handle open for writing, waits in the
    /// disk queue, and parks on a failed disk.
    async fn allocate(&self, len: u64) -> io::Result<()> {
        AllocateFuture::new(self.sim.clone(), self.handle_id, len).await
    }

    async fn drop_cache(&self) -> io::Result<()> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;
        sim.drop_file_cache(self.handle_id).map_err(Into::into)
    }
}

impl Drop for SimStorageFile {
    fn drop(&mut self) {
        // Release the handle in the engine. Operations in flight follow the
        // rules of a dropped future: a read is cancelled, a write still lands.
        if let Ok(sim) = self.sim.upgrade() {
            sim.close_storage_handle(self.handle_id);
        }
    }
}
