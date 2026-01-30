//! Simulated storage file implementation.

use crate::sim::WeakSimWorld;
use crate::sim::state::FileId;
use async_trait::async_trait;
use moonpool_core::StorageFile;
use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};

/// Simulated storage file for deterministic testing.
///
/// This provides a simulation-aware file handle that integrates with
/// the deterministic simulation engine for testing storage I/O patterns.
///
/// Note: Full async I/O implementation is deferred to Phase 7.
/// Current implementation provides stub methods that return errors.
#[derive(Debug)]
pub struct SimStorageFile {
    sim: WeakSimWorld,
    file_id: FileId,
}

impl SimStorageFile {
    /// Create a new simulated storage file.
    pub(crate) fn new(sim: WeakSimWorld, file_id: FileId) -> Self {
        Self { sim, file_id }
    }

    /// Get the file ID.
    pub fn file_id(&self) -> FileId {
        self.file_id
    }
}

#[async_trait(?Send)]
impl StorageFile for SimStorageFile {
    async fn sync_all(&self) -> io::Result<()> {
        let _sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;
        // TODO: Phase 7 - schedule sync event with latency
        Err(io::Error::other("sync_all not implemented"))
    }

    async fn sync_data(&self) -> io::Result<()> {
        let _sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;
        // TODO: Phase 7 - schedule sync event with latency
        Err(io::Error::other("sync_data not implemented"))
    }

    async fn size(&self) -> io::Result<u64> {
        let _sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;
        // TODO: Phase 7 - query file size from storage state
        Err(io::Error::other("size not implemented"))
    }

    async fn set_len(&self, _size: u64) -> io::Result<()> {
        let _sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;
        // TODO: Phase 7 - schedule set_len operation with latency
        Err(io::Error::other("set_len not implemented"))
    }
}

impl AsyncRead for SimStorageFile {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // TODO: Phase 7 - implement async read with simulation events
        Poll::Ready(Err(io::Error::other("poll_read not implemented")))
    }
}

impl AsyncWrite for SimStorageFile {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // TODO: Phase 7 - implement async write with simulation events
        Poll::Ready(Err(io::Error::other("poll_write not implemented")))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // TODO: Phase 7 - implement async flush with simulation events
        Poll::Ready(Err(io::Error::other("poll_flush not implemented")))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // TODO: Phase 7 - implement async shutdown
        Poll::Ready(Err(io::Error::other("poll_shutdown not implemented")))
    }
}

impl AsyncSeek for SimStorageFile {
    fn start_seek(self: Pin<&mut Self>, _position: SeekFrom) -> io::Result<()> {
        // TODO: Phase 7 - implement seek start
        Err(io::Error::other("start_seek not implemented"))
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        // TODO: Phase 7 - implement seek complete with simulation events
        Poll::Ready(Err(io::Error::other("poll_complete not implemented")))
    }
}
