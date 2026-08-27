//! Simulated [`BlockDevice`] / [`BlockDeviceProvider`] implementations.

use std::sync::Arc;

use moonpool_core::{BlockDevice, BlockDeviceProvider, BlockError, RegionId, RegionSpec};

use super::SimBlockStore;
use crate::executor::yield_now;

/// Simulated block-device provider backed by a shared [`SimBlockStore`].
///
/// Devices created through this provider follow the atomic-create contract:
/// they are invisible to [`open`](BlockDeviceProvider::open) until their first
/// successful `persist()`, and a crash before that erases them entirely.
#[derive(Debug, Clone)]
pub struct SimBlockDeviceProvider {
    store: SimBlockStore,
}

impl SimBlockDeviceProvider {
    /// Create a provider over the given store.
    #[must_use]
    pub fn new(store: SimBlockStore) -> Self {
        Self { store }
    }

    /// The backing store (fault injection, crash simulation, records).
    #[must_use]
    pub fn store(&self) -> &SimBlockStore {
        &self.store
    }
}

impl BlockDeviceProvider for SimBlockDeviceProvider {
    type Device = SimBlockDevice;

    async fn create(&self, path: &str, regions: &[RegionSpec]) -> Result<Self::Device, BlockError> {
        self.store.create_device(path, regions)?;
        yield_now().await;
        Ok(SimBlockDevice {
            store: self.store.clone(),
            path: Arc::from(path),
        })
    }

    async fn open(&self, path: &str) -> Result<Self::Device, BlockError> {
        yield_now().await;
        self.store.open_device(path)?;
        Ok(SimBlockDevice {
            store: self.store.clone(),
            path: Arc::from(path),
        })
    }
}

/// A simulated block device handle.
///
/// Every operation yields once to the deterministic executor between
/// validation and effect, so seeded task scheduling can interleave concurrent
/// I/O (which is also what makes the concurrent-overlapping-write assertion
/// meaningful).
#[derive(Debug, Clone)]
pub struct SimBlockDevice {
    store: SimBlockStore,
    path: Arc<str>,
}

impl SimBlockDevice {
    /// Path this device was created or opened at.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl BlockDevice for SimBlockDevice {
    async fn read(&self, region: RegionId, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.store
            .validate_read(&self.path, region, offset, buf.len())?;
        yield_now().await;
        self.store.finish_read(&self.path, region, offset, buf)
    }

    async fn write(&self, region: RegionId, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
        self.store
            .begin_write(&self.path, region, offset, buf.len())?;
        // The reservation must be released even if this future is dropped at
        // the yield point (e.g. inside a timeout), or the overlap assertion
        // would see a phantom in-flight write forever.
        let guard = WriteReservationGuard {
            store: &self.store,
            path: &self.path,
            region,
            offset,
            len: buf.len(),
        };
        yield_now().await;
        drop(guard);
        self.store.finish_write(&self.path, region, offset, buf)
    }

    async fn persist(&self) -> Result<(), BlockError> {
        yield_now().await;
        self.store.do_persist(&self.path)
    }

    async fn grow(&self, region: RegionId, new_size: u64) -> Result<(), BlockError> {
        yield_now().await;
        self.store.do_grow(&self.path, region, new_size)
    }

    fn region_size(&self, region: RegionId) -> u64 {
        self.store.region_size(&self.path, region)
    }

    fn region_count(&self) -> u32 {
        self.store.region_count(&self.path)
    }
}

/// Releases an in-flight write reservation on drop, so a cancelled write
/// future cannot leak it.
struct WriteReservationGuard<'a> {
    store: &'a SimBlockStore,
    path: &'a str,
    region: RegionId,
    offset: u64,
    len: usize,
}

impl Drop for WriteReservationGuard<'_> {
    fn drop(&mut self) {
        self.store
            .release_write_reservation(self.path, self.region, self.offset, self.len);
    }
}
