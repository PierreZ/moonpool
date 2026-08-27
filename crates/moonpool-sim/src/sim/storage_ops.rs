//! Thin `SimWorld` adapters for the storage simulation engine.

use std::{
    net::IpAddr,
    task::{Poll, Waker},
};

use tracing::instrument;

use crate::storage::{
    StorageConfiguration, StorageError,
    sim::{HandleId, OperationId, StorageActions, StorageCompletion, StorageEvent},
};

use super::{
    events::Event,
    wakers::WakeBatch,
    world::{SimInner, SimWorld},
};

/// Applies storage effects to the global scheduler and fault journal while the
/// world is locked. Returned wakers must run only after releasing that lock.
pub(crate) fn apply_storage_actions(inner: &mut SimInner, actions: StorageActions) -> WakeBatch {
    let StorageActions {
        scheduled,
        canceled,
        faults,
        mut wakes,
    } = actions;
    for scheduled in scheduled {
        let operation_id = scheduled.event.operation_id();
        match inner
            .scheduler
            .schedule_at(scheduled.at, Event::Storage(scheduled.event))
        {
            Ok(schedule_id) => {
                inner.storage_schedules.insert(operation_id, schedule_id);
            }
            Err(error) => {
                tracing::error!(%error, ?operation_id, "failed to schedule storage event");
                wakes.append(inner.storage.fail_scheduled_operation(operation_id));
            }
        }
    }
    for operation_id in canceled {
        if let Some(schedule_id) = inner.storage_schedules.remove(&operation_id) {
            inner.scheduler.cancel(schedule_id);
        }
    }
    for fault in faults {
        inner.record_fault(fault);
    }
    wakes
}

/// Handles one exact storage completion without invoking a waker under lock.
pub(crate) fn handle_storage_event(
    inner: &mut SimInner,
    event: StorageEvent,
    wakes: &mut WakeBatch,
) {
    let actions = inner.storage.handle_event(event);
    wakes.append(apply_storage_actions(inner, actions));
}

impl SimWorld {
    /// Access the default storage configuration for the simulation.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn with_storage_config<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&StorageConfiguration) -> R,
    {
        let inner = self.inner.read();
        f(inner.storage.config())
    }

    pub(crate) fn open_file(
        &self,
        path: &str,
        options: moonpool_core::OpenOptions,
        initial_size: u64,
        owner_ip: IpAddr,
    ) -> Result<HandleId, StorageError> {
        self.inner
            .write()
            .storage
            .open_file(path, options, initial_size, owner_ip)
    }

    pub(crate) fn file_exists(&self, path: &str) -> bool {
        self.inner.read().storage.file_exists(path)
    }

    pub(crate) fn delete_file(&self, path: &str) -> Result<(), StorageError> {
        let mut inner = self.inner.write();
        let actions = inner.storage.delete_file(path)?;
        let wakes = apply_storage_actions(&mut inner, actions);
        drop(inner);
        wakes.wake();
        Ok(())
    }

    pub(crate) fn rename_file(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let mut inner = self.inner.write();
        let actions = inner.storage.rename_file(from, to)?;
        let wakes = apply_storage_actions(&mut inner, actions);
        drop(inner);
        wakes.wake();
        Ok(())
    }

    pub(crate) fn schedule_read(
        &self,
        handle_id: HandleId,
        offset: u64,
        len: usize,
    ) -> Result<OperationId, StorageError> {
        let mut inner = self.inner.write();
        let now = inner.now();
        let (operation_id, actions) = inner.storage.schedule_read(handle_id, offset, len, now)?;
        let wakes = apply_storage_actions(&mut inner, actions);
        drop(inner);
        wakes.wake();
        Ok(operation_id)
    }

    pub(crate) fn schedule_write(
        &self,
        handle_id: HandleId,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<OperationId, StorageError> {
        let mut inner = self.inner.write();
        let now = inner.now();
        let (operation_id, actions) = inner.storage.schedule_write(handle_id, offset, data, now)?;
        let wakes = apply_storage_actions(&mut inner, actions);
        drop(inner);
        wakes.wake();
        Ok(operation_id)
    }

    pub(crate) fn schedule_sync(&self, handle_id: HandleId) -> Result<OperationId, StorageError> {
        let mut inner = self.inner.write();
        let now = inner.now();
        let (operation_id, actions) = inner.storage.schedule_sync(handle_id, now)?;
        let wakes = apply_storage_actions(&mut inner, actions);
        drop(inner);
        wakes.wake();
        Ok(operation_id)
    }

    pub(crate) fn schedule_set_len(
        &self,
        handle_id: HandleId,
        new_len: u64,
    ) -> Result<OperationId, StorageError> {
        let mut inner = self.inner.write();
        let now = inner.now();
        let (operation_id, actions) = inner.storage.schedule_set_len(handle_id, new_len, now)?;
        let wakes = apply_storage_actions(&mut inner, actions);
        drop(inner);
        wakes.wake();
        Ok(operation_id)
    }

    pub(crate) fn poll_storage_operation(
        &self,
        operation_id: OperationId,
        waker: &Waker,
    ) -> Poll<Result<StorageCompletion, StorageError>> {
        self.inner
            .write()
            .storage
            .poll_operation(operation_id, waker)
    }

    pub(crate) fn cancel_storage_operation(&self, operation_id: OperationId) {
        let mut inner = self.inner.write();
        let actions = inner.storage.cancel_operation(operation_id);
        let wakes = apply_storage_actions(&mut inner, actions);
        drop(inner);
        wakes.wake();
    }

    pub(crate) fn file_position(&self, handle_id: HandleId) -> Result<u64, StorageError> {
        self.inner.read().storage.file_position(handle_id)
    }

    pub(crate) fn set_file_position(
        &self,
        handle_id: HandleId,
        position: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .write()
            .storage
            .set_file_position(handle_id, position)
    }

    pub(crate) fn file_size(&self, handle_id: HandleId) -> Result<u64, StorageError> {
        self.inner.read().storage.file_size(handle_id)
    }

    pub(crate) fn close_storage_handle(&self, handle_id: HandleId) {
        let mut inner = self.inner.write();
        let actions = inner.storage.close_handle(handle_id);
        let wakes = apply_storage_actions(&mut inner, actions);
        drop(inner);
        wakes.wake();
    }

    /// Simulate a crash affecting storage for a specific process.
    ///
    /// Applies crash behavior to both the process's stream files and its
    /// block devices: every block-device sector written since the last
    /// `persist()` is resolved through the barrier-bounded crash model.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic, or if
    /// the block-device lost-synced-write oracle detects a simulator bug.
    #[instrument(skip(self))]
    pub fn simulate_crash_for_process(&self, ip: IpAddr, close_files: bool) {
        let (wakes, block_store) = {
            let mut inner = self.inner.write();
            let actions = inner.storage.simulate_crash(ip, close_files);
            let wakes = apply_storage_actions(&mut inner, actions);
            (wakes, inner.block.existing_store(ip))
        };
        wakes.wake();
        if let Some(store) = block_store {
            let reports = store.crash_all();
            if reports.iter().any(|report| report.existed) {
                self.inner
                    .write()
                    .record_fault(crate::chaos::SimFaultEvent::BlockDeviceCrash {
                        ip: ip.to_string(),
                    });
            }
        }
    }

    /// Wipe all persistent storage for a specific process, block devices
    /// included.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[instrument(skip(self))]
    pub fn wipe_storage_for_process(&self, ip: IpAddr) {
        let (wakes, block_store) = {
            let mut inner = self.inner.write();
            let actions = inner.storage.wipe_process(ip);
            let wakes = apply_storage_actions(&mut inner, actions);
            (wakes, inner.block.existing_store(ip))
        };
        wakes.wake();
        if let Some(store) = block_store
            && store.wipe_all() > 0
        {
            self.inner
                .write()
                .record_fault(crate::chaos::SimFaultEvent::BlockDeviceWipe { ip: ip.to_string() });
        }
    }

    /// Create a block-device provider scoped to a process IP.
    ///
    /// The per-process store is created lazily with a seed derived as a pure
    /// function of the iteration seed and the IP, so first use never shifts
    /// the counted sim RNG stream. Process crashes
    /// ([`simulate_crash_for_process`](Self::simulate_crash_for_process))
    /// resolve the store's buffered writes through the barrier-bounded crash
    /// model; wipes remove its devices.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn block_device_provider(
        &self,
        ip: IpAddr,
    ) -> crate::storage::block::SimBlockDeviceProvider {
        crate::storage::block::SimBlockDeviceProvider::new(self.block_store(ip))
    }

    /// The per-process block store backing
    /// [`block_device_provider`](Self::block_device_provider): targeted fault
    /// injection, crash reports, and fault records live here.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn block_store(&self, ip: IpAddr) -> crate::storage::block::SimBlockStore {
        self.inner.write().block.store_for(ip)
    }

    /// Replace the fault configuration for block stores created after this
    /// call. The builder applies the per-seed chaos configuration here before
    /// any process runs.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn set_block_fault_config(&self, config: crate::storage::block::BlockFaultConfig) {
        self.inner.write().block.set_config(config);
    }

    /// Set storage configuration for a specific process.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[instrument(skip(self, config))]
    pub fn set_process_storage_config(&self, ip: IpAddr, config: StorageConfiguration) {
        self.inner.write().storage.set_config_for(ip, config);
    }
}
