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
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[instrument(skip(self))]
    pub fn simulate_crash_for_process(&self, ip: IpAddr, close_files: bool) {
        let mut inner = self.inner.write();
        let actions = inner.storage.simulate_crash(ip, close_files);
        let wakes = apply_storage_actions(&mut inner, actions);
        drop(inner);
        wakes.wake();
    }

    /// Wipe all persistent storage for a specific process.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[instrument(skip(self))]
    pub fn wipe_storage_for_process(&self, ip: IpAddr) {
        let mut inner = self.inner.write();
        let actions = inner.storage.wipe_process(ip);
        let wakes = apply_storage_actions(&mut inner, actions);
        drop(inner);
        wakes.wake();
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
