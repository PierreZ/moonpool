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

    pub(crate) fn handle_constraints(
        &self,
        handle_id: HandleId,
    ) -> Result<moonpool_core::IoConstraints, StorageError> {
        self.inner.read().storage.handle_constraints(handle_id)
    }

    pub(crate) fn handle_is_direct_io(&self, handle_id: HandleId) -> Result<bool, StorageError> {
        self.inner.read().storage.handle_is_direct_io(handle_id)
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

    pub(crate) fn sync_dir(&self, path: &str, owner_ip: IpAddr) -> Result<(), StorageError> {
        let mut inner = self.inner.write();
        let actions = inner.storage.sync_dir(path, owner_ip)?;
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
        self.schedule_read_inner(handle_id, offset, len, false)
    }

    /// Schedule a positioned read: it addresses `offset` literally and leaves
    /// the handle's stream cursor alone.
    pub(crate) fn schedule_positioned_read(
        &self,
        handle_id: HandleId,
        offset: u64,
        len: usize,
    ) -> Result<OperationId, StorageError> {
        self.schedule_read_inner(handle_id, offset, len, true)
    }

    fn schedule_read_inner(
        &self,
        handle_id: HandleId,
        offset: u64,
        len: usize,
        positioned: bool,
    ) -> Result<OperationId, StorageError> {
        let mut inner = self.inner.write();
        let now = inner.now();
        let (operation_id, actions) = inner
            .storage
            .schedule_read(handle_id, offset, len, positioned, now)?;
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
        self.schedule_write_inner(handle_id, offset, data, false)
    }

    /// Schedule a positioned write: it lands at `offset` whatever the handle's
    /// append flag says, and leaves the stream cursor alone.
    pub(crate) fn schedule_positioned_write(
        &self,
        handle_id: HandleId,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<OperationId, StorageError> {
        self.schedule_write_inner(handle_id, offset, data, true)
    }

    fn schedule_write_inner(
        &self,
        handle_id: HandleId,
        offset: u64,
        data: Vec<u8>,
        positioned: bool,
    ) -> Result<OperationId, StorageError> {
        let mut inner = self.inner.write();
        let now = inner.now();
        let (operation_id, actions) = inner
            .storage
            .schedule_write(handle_id, offset, data, positioned, now)?;
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
    /// Every sector written since the last successful sync resolves through
    /// the barrier-bounded crash model — kept old, kept new, lost, damaged, or
    /// shorn — and every unsynced directory entry resolves against the durable
    /// namespace. What each file's sectors did is available from
    /// [`take_storage_crash_reports`](Self::take_storage_crash_reports).
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic, or if
    /// the lost-synced-write oracle detects a simulator bug.
    #[instrument(skip(self))]
    pub fn simulate_crash_for_process(&self, ip: IpAddr, close_files: bool) {
        let wakes = {
            let mut inner = self.inner.write();
            let actions = inner.storage.simulate_crash(ip, close_files);
            apply_storage_actions(&mut inner, actions)
        };
        wakes.wake();
    }

    /// Fail `ip`'s disk outright: every read, write, sync, or `set_len` issued
    /// to a file it owns from now on is accepted and never completes, until
    /// [`simulate_crash_for_process`](Self::simulate_crash_for_process) or
    /// [`wipe_storage_for_process`](Self::wipe_storage_for_process) replaces
    /// the disk. This is the scripted counterpart of
    /// [`StorageConfiguration::disk_failure_probability`](crate::StorageConfiguration::disk_failure_probability):
    /// it consumes no randomness and ignores the coin's one-at-a-time budget.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[instrument(skip(self))]
    pub fn fail_disk_for_process(&self, ip: IpAddr) {
        let wakes = {
            let mut inner = self.inner.write();
            let actions = inner.storage.fail_disk(ip);
            apply_storage_actions(&mut inner, actions)
        };
        wakes.wake();
    }

    /// Wipe all persistent storage for a specific process.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[instrument(skip(self))]
    pub fn wipe_storage_for_process(&self, ip: IpAddr) {
        let wakes = {
            let mut inner = self.inner.write();
            let actions = inner.storage.wipe_process(ip);
            apply_storage_actions(&mut inner, actions)
        };
        wakes.wake();
    }

    /// Install the eligibility mask consulted before any random fault damages
    /// a sector (see
    /// [`StorageEligibilityMask`](crate::storage::StorageEligibilityMask)).
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn set_storage_eligibility_mask(&self, mask: crate::storage::StorageEligibilityMask) {
        self.inner.write().storage.set_eligibility_mask(Some(mask));
    }

    /// Remove the eligibility mask: every sector becomes eligible again.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn clear_storage_eligibility_mask(&self) {
        self.inner.write().storage.set_eligibility_mask(None);
    }

    /// Drain the storage faults injected so far, oldest first.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn take_storage_fault_records(&self) -> Vec<crate::storage::StorageFaultRecord> {
        self.inner.write().storage.take_fault_records()
    }

    /// Drain the per-file reports of the crashes simulated so far: how each
    /// sector dirty at crash time resolved.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn take_storage_crash_reports(&self) -> Vec<crate::storage::FileCrashReport> {
        self.inner.write().storage.take_crash_reports()
    }

    /// Plant a latent read fault on `sectors` of `path`: reads return
    /// deterministically corrupted bytes, identically on every retry, until
    /// the sectors are rewritten.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotFound`] if no file exists at `path`.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn corrupt_file(
        &self,
        path: &str,
        sectors: std::ops::Range<u64>,
    ) -> Result<(), StorageError> {
        self.inner.write().storage.corrupt_file(path, sectors)
    }

    /// Make reads and/or writes touching `sectors` of `path` fail with an I/O
    /// error until [`clear_file_eio`](Self::clear_file_eio) is called. An
    /// error is an operating condition, not corrupt bytes.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotFound`] if no file exists at `path`.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn fail_file_with_eio(
        &self,
        path: &str,
        sectors: std::ops::Range<u64>,
        target: crate::storage::EioTarget,
    ) -> Result<(), StorageError> {
        self.inner
            .write()
            .storage
            .fail_file_with_eio(path, sectors, target)
    }

    /// Clear targeted EIO injections on `path`.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotFound`] if no file exists at `path`.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn clear_file_eio(
        &self,
        path: &str,
        target: crate::storage::EioTarget,
    ) -> Result<(), StorageError> {
        self.inner.write().storage.clear_file_eio(path, target)
    }

    /// Mutate a *durable* sector of `path` out of band, bypassing the crash
    /// model.
    ///
    /// A deliberate simulator bug: the next crash of that process must fail
    /// loudly, because a sector a sync reported durable changed underneath.
    /// It exists so the lost-synced-write oracle can itself be tested.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotFound`] if no file exists at `path`.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn corrupt_durable_out_of_band(&self, path: &str, sector: u64) -> Result<(), StorageError> {
        self.inner
            .write()
            .storage
            .corrupt_durable_out_of_band(path, sector)
    }

    /// Set storage configuration for a specific process.
    ///
    /// Recovery-aware in the same way as
    /// [`set_storage_config`](Self::set_storage_config).
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[instrument(skip(self, config))]
    pub fn set_process_storage_config(&self, ip: IpAddr, mut config: StorageConfiguration) {
        let mut inner = self.inner.write();
        if inner.recovery_mode() {
            config.disable_fault_injection();
        }
        inner.storage.set_config_for(ip, config);
    }
}
