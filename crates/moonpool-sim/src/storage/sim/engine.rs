//! Storage state transitions and fault injection.

use std::{
    collections::BTreeMap,
    net::IpAddr,
    task::{Poll, Waker},
    time::Duration,
};

use moonpool_core::OpenOptions;

use super::{
    DiskDegradationState, DiskEpisodeKind, FileId, HandleId, OperationId, StorageEvent,
    state::{FileState, HandleState, PendingOpType, PendingStorageOp, StorageState},
};
use crate::{
    assert_reachable,
    chaos::fault_events::SimFaultEvent,
    network::sample_latency,
    sim::{
        rng::{sim_random, sim_random_range},
        wakers::{WakeBatch, WakerRegistry},
    },
    storage::{InMemoryStorage, StorageConfiguration, StorageError, StorageOperation},
};

/// One storage event requested at an absolute simulation time.
#[derive(Debug)]
pub(crate) struct ScheduledStorageEvent {
    pub(crate) at: Duration,
    pub(crate) event: StorageEvent,
}

/// Ordered effects produced by a storage state transition.
#[derive(Debug, Default)]
pub(crate) struct StorageActions {
    pub(crate) scheduled: Vec<ScheduledStorageEvent>,
    pub(crate) canceled: Vec<OperationId>,
    pub(crate) faults: Vec<SimFaultEvent>,
    pub(crate) wakes: WakeBatch,
}

/// Value captured when one exact storage operation completes.
#[derive(Debug)]
pub(crate) enum StorageCompletion {
    /// Bytes observed when a read event was handled.
    Read(Vec<u8>),
    /// Location and length committed by a write event.
    Write { offset: u64, len: usize },
    /// Completion for operations without a value.
    Unit,
}

impl StorageActions {
    fn schedule(&mut self, at: Duration, event: StorageEvent) {
        self.scheduled.push(ScheduledStorageEvent { at, event });
    }

    fn fault(&mut self, event: SimFaultEvent) {
        self.faults.push(event);
    }

    fn cancel(&mut self, operation_id: OperationId) {
        self.canceled.push(operation_id);
    }
}

/// Owns all deterministic simulated-storage state and waiters.
#[derive(Debug)]
pub struct StorageEngine {
    state: StorageState,
    results: BTreeMap<OperationId, Result<StorageCompletion, StorageError>>,
    wakers: WakerRegistry<OperationId>,
}

impl StorageEngine {
    /// Creates an engine using `config` as its default disk profile.
    #[must_use]
    pub fn new(config: StorageConfiguration) -> Self {
        Self {
            state: StorageState::new(config),
            results: BTreeMap::new(),
            wakers: WakerRegistry::default(),
        }
    }

    pub(crate) fn config(&self) -> &StorageConfiguration {
        &self.state.config
    }

    pub(crate) fn set_config(&mut self, config: StorageConfiguration) {
        self.state.config = config;
    }

    pub(crate) fn set_config_for(&mut self, ip: IpAddr, config: StorageConfiguration) {
        self.state.per_process_configs.insert(ip, config);
    }

    /// Stop sampling new storage faults on every disk, default and per-process
    /// alike (see
    /// [`StorageConfiguration::disable_fault_injection`](crate::storage::StorageConfiguration::disable_fault_injection)).
    ///
    /// Disk-degradation episodes already in force are deliberately left in
    /// `disk_episodes`: they carry an expiry and clear themselves on the next
    /// operation past it, so a stall that started under chaos still has to be
    /// waited out.
    pub(crate) fn disable_fault_injection(&mut self) {
        self.state.config.disable_fault_injection();
        for config in self.state.per_process_configs.values_mut() {
            config.disable_fault_injection();
        }
    }

    pub(crate) fn disk_episode_for(&self, ip: IpAddr) -> Option<DiskDegradationState> {
        self.state.disk_episodes.get(&ip).copied()
    }

    pub(crate) fn open_file(
        &mut self,
        path: &str,
        options: OpenOptions,
        initial_size: u64,
        owner_ip: IpAddr,
    ) -> Result<HandleId, StorageError> {
        let path = path.to_string();
        if options.is_create_new() && self.state.path_to_file.contains_key(&path) {
            return Err(StorageError::AlreadyExists { path });
        }

        let file_id = if let Some(existing_id) = self.state.path_to_file.get(&path).copied() {
            if options.is_truncate()
                && let Some(file) = self.state.files.get_mut(&existing_id)
            {
                file.storage = InMemoryStorage::new(0, sim_random::<u64>());
            }
            existing_id
        } else {
            if !options.is_create() && !options.is_create_new() {
                return Err(StorageError::NotFound { path });
            }
            let file_id = FileId(self.state.next_file_id);
            self.state.next_file_id += 1;
            self.state.files.insert(
                file_id,
                FileState {
                    path: path.clone(),
                    storage: InMemoryStorage::new(initial_size, sim_random::<u64>()),
                    owner_ip,
                },
            );
            self.state.path_to_file.insert(path, file_id);
            file_id
        };

        let position = if options.is_append() {
            self.state
                .files
                .get(&file_id)
                .map_or(0, |file| file.storage.size())
        } else {
            0
        };
        let handle_id = HandleId(self.state.next_handle_id);
        self.state.next_handle_id += 1;
        self.state
            .handles
            .insert(handle_id, HandleState::new(file_id, position, options));
        Ok(handle_id)
    }

    pub(crate) fn file_exists(&self, path: &str) -> bool {
        self.state.path_to_file.contains_key(path)
    }

    pub(crate) fn delete_file(&mut self, path: &str) -> Result<StorageActions, StorageError> {
        let Some(file_id) = self.state.path_to_file.remove(path) else {
            return Err(StorageError::NotFound {
                path: path.to_string(),
            });
        };
        self.state.files.remove(&file_id);
        let mut actions = StorageActions::default();
        self.invalidate_file_handles(file_id, &mut actions, false);
        Ok(actions)
    }

    pub(crate) fn rename_file(
        &mut self,
        from: &str,
        to: &str,
    ) -> Result<StorageActions, StorageError> {
        if from == to {
            return self
                .state
                .path_to_file
                .contains_key(from)
                .then(StorageActions::default)
                .ok_or_else(|| StorageError::NotFound {
                    path: from.to_string(),
                });
        }
        let Some(file_id) = self.state.path_to_file.remove(from) else {
            return Err(StorageError::NotFound {
                path: from.to_string(),
            });
        };
        let mut actions = StorageActions::default();
        if let Some(replaced_id) = self.state.path_to_file.remove(to) {
            self.state.files.remove(&replaced_id);
            self.invalidate_file_handles(replaced_id, &mut actions, false);
        }
        if let Some(file) = self.state.files.get_mut(&file_id) {
            file.path = to.to_string();
        }
        self.state.path_to_file.insert(to.to_string(), file_id);
        Ok(actions)
    }

    pub(crate) fn schedule_read(
        &mut self,
        handle_id: HandleId,
        offset: u64,
        len: usize,
        now: Duration,
    ) -> Result<(OperationId, StorageActions), StorageError> {
        self.ensure_readable(handle_id)?;
        let file_id = self.open_file_id(handle_id)?;
        let owner_ip = self.owner_ip(file_id)?;
        let episode = self.update_disk_episode(owner_ip, now);
        let latency = Self::calculate_storage_latency(
            self.state.config_for(owner_ip),
            len,
            false,
            episode,
            now,
        );
        self.schedule_operation(
            PendingStorageOp {
                handle_id,
                file_id,
                op_type: PendingOpType::Read,
                offset,
                len,
                data: None,
                append: false,
            },
            StorageOperation::ReadComplete {
                len: u32::try_from(len).expect("read length fits in u32"),
            },
            now.saturating_add(latency),
        )
    }

    pub(crate) fn schedule_write(
        &mut self,
        handle_id: HandleId,
        offset: u64,
        data: Vec<u8>,
        now: Duration,
    ) -> Result<(OperationId, StorageActions), StorageError> {
        self.ensure_writable(handle_id, "write")?;
        let file_id = self.open_file_id(handle_id)?;
        let append = self.open_handle(handle_id)?.options.is_append();
        let owner_ip = self.owner_ip(file_id)?;
        let len = data.len();
        let episode = self.update_disk_episode(owner_ip, now);
        let latency = Self::calculate_storage_latency(
            self.state.config_for(owner_ip),
            len,
            true,
            episode,
            now,
        );
        self.schedule_operation(
            PendingStorageOp {
                handle_id,
                file_id,
                op_type: PendingOpType::Write,
                offset,
                len,
                data: Some(data),
                append,
            },
            StorageOperation::WriteComplete {
                len: u32::try_from(len).expect("write length fits in u32"),
            },
            now.saturating_add(latency),
        )
    }

    pub(crate) fn schedule_sync(
        &mut self,
        handle_id: HandleId,
        now: Duration,
    ) -> Result<(OperationId, StorageActions), StorageError> {
        let file_id = self.open_file_id(handle_id)?;
        let owner_ip = self.owner_ip(file_id)?;
        let episode = self.update_disk_episode(owner_ip, now);
        let mut latency = sample_latency(&self.state.config_for(owner_ip).sync_latency);
        if let Some(DiskDegradationState {
            kind: DiskEpisodeKind::Stall,
            expires_at,
        }) = episode
        {
            latency = latency.saturating_add(expires_at.saturating_sub(now));
        }
        self.schedule_operation(
            PendingStorageOp {
                handle_id,
                file_id,
                op_type: PendingOpType::Sync,
                offset: 0,
                len: 0,
                data: None,
                append: false,
            },
            StorageOperation::SyncComplete,
            now.saturating_add(latency),
        )
    }

    pub(crate) fn schedule_set_len(
        &mut self,
        handle_id: HandleId,
        new_len: u64,
        now: Duration,
    ) -> Result<(OperationId, StorageActions), StorageError> {
        self.ensure_writable(handle_id, "set_len")?;
        let file_id = self.open_file_id(handle_id)?;
        let owner_ip = self.owner_ip(file_id)?;
        let latency = sample_latency(&self.state.config_for(owner_ip).write_latency);
        self.schedule_operation(
            PendingStorageOp {
                handle_id,
                file_id,
                op_type: PendingOpType::SetLen,
                offset: new_len,
                len: 0,
                data: None,
                append: false,
            },
            StorageOperation::SetLenComplete { new_len },
            now.saturating_add(latency),
        )
    }

    fn schedule_operation(
        &mut self,
        pending: PendingStorageOp,
        operation: StorageOperation,
        at: Duration,
    ) -> Result<(OperationId, StorageActions), StorageError> {
        let operation_id = OperationId(self.state.next_operation_id);
        self.state.next_operation_id += 1;
        let handle = self.open_handle_mut(pending.handle_id)?;
        handle.pending_ops.insert(operation_id);
        let handle_id = pending.handle_id;
        self.state.pending_ops.insert(operation_id, pending);
        let mut actions = StorageActions::default();
        actions.schedule(at, StorageEvent::new(operation_id, handle_id, operation));
        Ok((operation_id, actions))
    }

    pub(crate) fn handle_event(&mut self, event: StorageEvent) -> StorageActions {
        let mut actions = StorageActions::default();
        let Some(pending) = self.state.pending_ops.remove(&event.operation_id()) else {
            return actions;
        };
        if pending.handle_id != event.handle_id() {
            tracing::warn!(
                operation_id = ?event.operation_id(),
                expected_handle = ?pending.handle_id,
                actual_handle = ?event.handle_id(),
                "storage completion targeted the wrong handle"
            );
            self.state.pending_ops.insert(event.operation_id(), pending);
            return actions;
        }
        if let Some(handle) = self.state.handles.get_mut(&pending.handle_id) {
            handle.pending_ops.remove(&event.operation_id());
        }

        let result = match (pending.op_type, event.operation()) {
            (PendingOpType::Read, StorageOperation::ReadComplete { .. }) => {
                self.complete_read(&pending, &mut actions)
            }
            (PendingOpType::Write, StorageOperation::WriteComplete { .. }) => {
                self.complete_write(event.operation_id(), pending, &mut actions)
            }
            (PendingOpType::Sync, StorageOperation::SyncComplete) => self
                .complete_sync(&pending, &mut actions)
                .map(|()| StorageCompletion::Unit),
            (PendingOpType::SetLen, StorageOperation::SetLenComplete { new_len }) => self
                .complete_set_len(&pending, new_len)
                .map(|()| StorageCompletion::Unit),
            _ => Err(StorageError::InvalidOperation {
                operation_id: event.operation_id(),
            }),
        };
        self.results.insert(event.operation_id(), result);
        actions.wakes.push(self.wakers.take(&event.operation_id()));
        actions
    }

    fn complete_read(
        &mut self,
        pending: &PendingStorageOp,
        actions: &mut StorageActions,
    ) -> Result<StorageCompletion, StorageError> {
        let (owner_ip, file_size, config) = self
            .state
            .files
            .get(&pending.file_id)
            .map(|file| {
                (
                    file.owner_ip,
                    file.storage.size(),
                    self.state.config_for(file.owner_ip).clone(),
                )
            })
            .ok_or(StorageError::InvalidFileHandle {
                handle_id: pending.handle_id,
            })?;
        let mut read_faulted = false;
        if config.read_fault_probability > 0.0
            && let Some(file) = self.state.files.get_mut(&pending.file_id)
        {
            let offset = usize::try_from(pending.offset).expect("offset fits in usize");
            let first_sector = offset / crate::storage::SECTOR_SIZE;
            let end_sector = (offset + pending.len).div_ceil(crate::storage::SECTOR_SIZE);
            for sector in first_sector..end_sector {
                if sim_random::<f64>() < config.read_fault_probability {
                    file.storage.set_fault(sector);
                    read_faulted = true;
                }
            }
        }

        let max_offset = file_size.saturating_sub(pending.len as u64);
        let mut read_offset = pending.offset;
        let mut misdirected = false;
        if config.misdirect_read_probability > 0.0
            && sim_random::<f64>() < config.misdirect_read_probability
            && max_offset > 0
        {
            let positions = max_offset + 1;
            let original_position = pending.offset % positions;
            let delta = sim_random_range(1..positions);
            read_offset = if original_position >= positions - delta {
                original_position - (positions - delta)
            } else {
                original_position + delta
            };
            misdirected = read_offset != pending.offset;
        }

        let mut data = vec![0; pending.len];
        let file =
            self.state
                .files
                .get(&pending.file_id)
                .ok_or(StorageError::InvalidFileHandle {
                    handle_id: pending.handle_id,
                })?;
        file.storage
            .read(read_offset, &mut data)
            .map_err(|error| StorageError::Io {
                file_id: pending.file_id,
                kind: error.kind(),
                message: error.to_string(),
            })?;

        if read_faulted || misdirected {
            actions.fault(SimFaultEvent::StorageReadFault {
                ip: owner_ip.to_string(),
                file_id: pending.file_id.0,
            });
        }
        Ok(StorageCompletion::Read(data))
    }

    fn complete_write(
        &mut self,
        operation_id: OperationId,
        pending: PendingStorageOp,
        actions: &mut StorageActions,
    ) -> Result<StorageCompletion, StorageError> {
        let Some(data) = pending.data else {
            return Err(StorageError::InvalidOperationData { operation_id });
        };
        let config = self
            .state
            .files
            .get(&pending.file_id)
            .map(|file| self.state.config_for(file.owner_ip).clone())
            .unwrap_or_default();
        let Some(file) = self.state.files.get_mut(&pending.file_id) else {
            return Err(StorageError::InvalidFileHandle {
                handle_id: pending.handle_id,
            });
        };
        let offset = if pending.append {
            file.storage.size()
        } else {
            pending.offset
        };
        let mut fault_kind = None;
        if sim_random::<f64>() < config.phantom_write_probability {
            file.storage.record_phantom_write(offset, &data);
            fault_kind = Some("phantom");
        } else if sim_random::<f64>() < config.misdirect_write_probability {
            let max_offset = file.storage.size().saturating_sub(data.len() as u64);
            let mistaken_offset = if max_offset > 0 {
                sim_random_range(0..max_offset)
            } else {
                0
            };
            file.storage
                .apply_misdirected_write(offset, mistaken_offset, &data)
                .map_err(|error| StorageError::Io {
                    file_id: pending.file_id,
                    kind: error.kind(),
                    message: error.to_string(),
                })?;
            fault_kind = Some("misdirected");
        } else {
            file.storage
                .write(offset, &data, false)
                .map_err(|error| StorageError::Io {
                    file_id: pending.file_id,
                    kind: error.kind(),
                    message: error.to_string(),
                })?;
            if config.write_fault_probability > 0.0 {
                let offset_usize = usize::try_from(offset).expect("offset fits in usize");
                let first_sector = offset_usize / crate::storage::SECTOR_SIZE;
                let end_sector = (offset_usize + data.len()).div_ceil(crate::storage::SECTOR_SIZE);
                for sector in first_sector..end_sector {
                    if sim_random::<f64>() < config.write_fault_probability {
                        file.storage.set_fault(sector);
                        fault_kind = Some("corruption");
                    }
                }
            }
        }
        if let Some(write_kind) = fault_kind {
            actions.fault(SimFaultEvent::StorageWriteFault {
                ip: file.owner_ip.to_string(),
                file_id: pending.file_id.0,
                write_kind: write_kind.to_string(),
            });
        }
        let len = data.len();
        if let Some(handle) = self.state.handles.get_mut(&pending.handle_id) {
            handle.position = offset + len as u64;
        }
        Ok(StorageCompletion::Write { offset, len })
    }

    fn complete_sync(
        &mut self,
        pending: &PendingStorageOp,
        actions: &mut StorageActions,
    ) -> Result<(), StorageError> {
        let probability = self.state.files.get(&pending.file_id).map_or(0.0, |file| {
            self.state
                .config_for(file.owner_ip)
                .sync_failure_probability
        });
        if sim_random::<f64>() < probability {
            let ip = self
                .state
                .files
                .get(&pending.file_id)
                .map(|file| file.owner_ip.to_string())
                .unwrap_or_default();
            actions.fault(SimFaultEvent::StorageSyncFault {
                ip,
                file_id: pending.file_id.0,
            });
            return Err(StorageError::Io {
                file_id: pending.file_id,
                kind: std::io::ErrorKind::Other,
                message: "sync failed (simulated I/O error)".to_string(),
            });
        }
        let Some(file) = self.state.files.get_mut(&pending.file_id) else {
            return Err(StorageError::InvalidFileHandle {
                handle_id: pending.handle_id,
            });
        };
        file.storage.sync();
        Ok(())
    }

    fn complete_set_len(
        &mut self,
        pending: &PendingStorageOp,
        new_len: u64,
    ) -> Result<(), StorageError> {
        let Some(file) = self.state.files.get_mut(&pending.file_id) else {
            return Err(StorageError::InvalidFileHandle {
                handle_id: pending.handle_id,
            });
        };
        file.storage.resize(new_len);
        Ok(())
    }

    pub(crate) fn poll_operation(
        &mut self,
        operation_id: OperationId,
        waker: &Waker,
    ) -> Poll<Result<StorageCompletion, StorageError>> {
        if let Some(result) = self.results.remove(&operation_id) {
            return Poll::Ready(result);
        }
        if self.state.pending_ops.contains_key(&operation_id) {
            self.wakers.register(operation_id, waker);
            Poll::Pending
        } else {
            Poll::Ready(Err(StorageError::InvalidOperation { operation_id }))
        }
    }

    pub(crate) fn cancel_operation(&mut self, operation_id: OperationId) -> StorageActions {
        let mut actions = StorageActions::default();
        if let Some(pending) = self.state.pending_ops.remove(&operation_id) {
            if let Some(handle) = self.state.handles.get_mut(&pending.handle_id) {
                handle.pending_ops.remove(&operation_id);
            }
            actions.cancel(operation_id);
        }
        self.results.remove(&operation_id);
        self.wakers.take(&operation_id);
        actions
    }

    pub(crate) fn fail_scheduled_operation(&mut self, operation_id: OperationId) -> WakeBatch {
        let Some(pending) = self.state.pending_ops.remove(&operation_id) else {
            return WakeBatch::default();
        };
        if let Some(handle) = self.state.handles.get_mut(&pending.handle_id) {
            handle.pending_ops.remove(&operation_id);
        }
        self.results.insert(
            operation_id,
            Err(StorageError::ScheduleFailed { operation_id }),
        );
        let mut wakes = WakeBatch::default();
        wakes.push(self.wakers.take(&operation_id));
        wakes
    }

    pub(crate) fn file_position(&self, handle_id: HandleId) -> Result<u64, StorageError> {
        Ok(self.open_handle(handle_id)?.position)
    }

    pub(crate) fn set_file_position(
        &mut self,
        handle_id: HandleId,
        position: u64,
    ) -> Result<(), StorageError> {
        self.open_handle_mut(handle_id)?.position = position;
        Ok(())
    }

    pub(crate) fn file_size(&self, handle_id: HandleId) -> Result<u64, StorageError> {
        let file_id = self.open_file_id(handle_id)?;
        self.state
            .files
            .get(&file_id)
            .map(|file| file.storage.size())
            .ok_or(StorageError::InvalidFileHandle { handle_id })
    }

    pub(crate) fn close_handle(&mut self, handle_id: HandleId) -> StorageActions {
        let mut actions = StorageActions::default();
        let Some(handle) = self.state.handles.remove(&handle_id) else {
            return actions;
        };
        for operation_id in handle.pending_ops {
            self.state.pending_ops.remove(&operation_id);
            self.results.remove(&operation_id);
            actions.cancel(operation_id);
            actions.wakes.push(self.wakers.take(&operation_id));
        }
        actions
    }

    pub(crate) fn simulate_crash(&mut self, ip: IpAddr, close_files: bool) -> StorageActions {
        let probability = self.state.config_for(ip).crash_fault_probability;
        let file_ids = self
            .state
            .files
            .iter()
            .filter_map(|(id, file)| (file.owner_ip == ip).then_some(*id))
            .collect::<Vec<_>>();
        for file_id in &file_ids {
            if let Some(file) = self.state.files.get_mut(file_id) {
                file.storage.apply_crash(probability);
            }
        }

        let mut actions = StorageActions::default();
        let handle_ids = self
            .state
            .handles
            .iter()
            .filter_map(|(id, handle)| file_ids.contains(&handle.file_id).then_some(*id))
            .collect::<Vec<_>>();
        for handle_id in handle_ids {
            self.fail_handle_operations(handle_id, &mut actions, close_files);
        }
        actions.fault(SimFaultEvent::StorageCrash { ip: ip.to_string() });
        actions
    }

    pub(crate) fn wipe_process(&mut self, ip: IpAddr) -> StorageActions {
        let files = self
            .state
            .files
            .iter()
            .filter(|(_, file)| file.owner_ip == ip)
            .map(|(id, file)| (*id, file.path.clone()))
            .collect::<Vec<_>>();
        let mut actions = StorageActions::default();
        for (file_id, path) in files {
            self.state.files.remove(&file_id);
            self.state.path_to_file.remove(&path);
            self.invalidate_file_handles(file_id, &mut actions, true);
        }
        actions.fault(SimFaultEvent::StorageWipe { ip: ip.to_string() });
        actions
    }

    pub(crate) fn shutdown(&mut self) -> StorageActions {
        let mut actions = StorageActions::default();
        for (operation_id, result) in &mut self.results {
            *result = Err(StorageError::SimulationShutdown {
                operation_id: *operation_id,
            });
        }
        let operation_ids = self.state.pending_ops.keys().copied().collect::<Vec<_>>();
        for operation_id in operation_ids {
            let Some(pending) = self.state.pending_ops.remove(&operation_id) else {
                continue;
            };
            if let Some(handle) = self.state.handles.get_mut(&pending.handle_id) {
                handle.pending_ops.remove(&operation_id);
                handle.is_closed = true;
            }
            self.results.insert(
                operation_id,
                Err(StorageError::SimulationShutdown { operation_id }),
            );
            actions.cancel(operation_id);
            actions.wakes.push(self.wakers.take(&operation_id));
        }
        for handle in self.state.handles.values_mut() {
            handle.is_closed = true;
        }
        actions
            .wakes
            .extend(self.wakers.drain().map(|(_, waker)| waker));
        actions
    }

    fn fail_handle_operations(
        &mut self,
        handle_id: HandleId,
        actions: &mut StorageActions,
        close_handle: bool,
    ) {
        let Some(handle) = self.state.handles.get_mut(&handle_id) else {
            return;
        };
        let file_id = handle.file_id;
        let operation_ids = std::mem::take(&mut handle.pending_ops);
        handle.is_closed |= close_handle;
        for operation_id in operation_ids {
            self.state.pending_ops.remove(&operation_id);
            self.results.insert(
                operation_id,
                Err(StorageError::OperationInterrupted {
                    operation_id,
                    file_id,
                }),
            );
            actions.cancel(operation_id);
            actions.wakes.push(self.wakers.take(&operation_id));
        }
    }

    fn invalidate_file_handles(
        &mut self,
        file_id: FileId,
        actions: &mut StorageActions,
        remove_handles: bool,
    ) {
        let handle_ids = self
            .state
            .handles
            .iter()
            .filter_map(|(id, handle)| (handle.file_id == file_id).then_some(*id))
            .collect::<Vec<_>>();
        for handle_id in handle_ids {
            self.fail_handle_operations(handle_id, actions, true);
            if remove_handles {
                self.state.handles.remove(&handle_id);
            }
        }
    }

    fn open_handle(&self, handle_id: HandleId) -> Result<&HandleState, StorageError> {
        let handle = self
            .state
            .handles
            .get(&handle_id)
            .ok_or(StorageError::InvalidFileHandle { handle_id })?;
        if handle.is_closed {
            return Err(StorageError::FileClosed { handle_id });
        }
        Ok(handle)
    }

    fn open_handle_mut(&mut self, handle_id: HandleId) -> Result<&mut HandleState, StorageError> {
        let handle = self
            .state
            .handles
            .get_mut(&handle_id)
            .ok_or(StorageError::InvalidFileHandle { handle_id })?;
        if handle.is_closed {
            return Err(StorageError::FileClosed { handle_id });
        }
        Ok(handle)
    }

    fn open_file_id(&self, handle_id: HandleId) -> Result<FileId, StorageError> {
        Ok(self.open_handle(handle_id)?.file_id)
    }

    fn ensure_readable(&self, handle_id: HandleId) -> Result<(), StorageError> {
        if self.open_handle(handle_id)?.options.is_read() {
            Ok(())
        } else {
            Err(StorageError::PermissionDenied {
                handle_id,
                operation: "read",
            })
        }
    }

    fn ensure_writable(
        &self,
        handle_id: HandleId,
        operation: &'static str,
    ) -> Result<(), StorageError> {
        let options = &self.open_handle(handle_id)?.options;
        if options.is_write() || options.is_append() {
            Ok(())
        } else {
            Err(StorageError::PermissionDenied {
                handle_id,
                operation,
            })
        }
    }

    fn owner_ip(&self, file_id: FileId) -> Result<IpAddr, StorageError> {
        self.state
            .files
            .get(&file_id)
            .map(|file| file.owner_ip)
            .ok_or(StorageError::MissingFile { file_id })
    }

    fn update_disk_episode(
        &mut self,
        owner_ip: IpAddr,
        now: Duration,
    ) -> Option<DiskDegradationState> {
        if self
            .state
            .disk_episodes
            .get(&owner_ip)
            .is_some_and(|episode| now >= episode.expires_at)
        {
            self.state.disk_episodes.remove(&owner_ip);
        }
        if let Some(episode) = self.state.disk_episodes.get(&owner_ip).copied() {
            return Some(episode);
        }
        let config = self.state.config_for(owner_ip);
        let knobs = (
            config.disk_stall_probability,
            config.disk_stall_duration,
            config.disk_throttle_probability,
            config.disk_throttle_duration,
        );
        if knobs.0 <= 0.0 && knobs.2 <= 0.0 {
            return None;
        }
        let roll = sim_random::<f64>();
        let episode = if roll < knobs.0 {
            assert_reachable!("disk: stall episode entered");
            Some(DiskDegradationState {
                kind: DiskEpisodeKind::Stall,
                expires_at: now.saturating_add(knobs.1),
            })
        } else if roll < knobs.0 + knobs.2 {
            assert_reachable!("disk: throttle episode entered");
            Some(DiskDegradationState {
                kind: DiskEpisodeKind::Throttle,
                expires_at: now.saturating_add(knobs.3),
            })
        } else {
            None
        };
        if let Some(episode) = episode {
            self.state.disk_episodes.insert(owner_ip, episode);
        }
        episode
    }

    fn calculate_storage_latency(
        config: &StorageConfiguration,
        size: usize,
        is_write: bool,
        episode: Option<DiskDegradationState>,
        now: Duration,
    ) -> Duration {
        let base = sample_latency(if is_write {
            &config.write_latency
        } else {
            &config.read_latency
        });
        let (iops_divisor, bandwidth_divisor) = match episode {
            Some(DiskDegradationState {
                kind: DiskEpisodeKind::Throttle,
                ..
            }) => (
                config.disk_throttle_iops_multiplier.max(1.0),
                config.disk_throttle_bandwidth_multiplier.max(1.0),
            ),
            _ => (1.0, 1.0),
        };
        let iops = u32::try_from(config.iops).map_or(f64::from(u32::MAX), f64::from);
        let size = u32::try_from(size).map_or(f64::from(u32::MAX), f64::from);
        let bandwidth = u32::try_from(config.bandwidth).map_or(f64::from(u32::MAX), f64::from);
        let steady = base
            .saturating_add(saturating_duration_from_secs(iops_divisor / iops))
            .saturating_add(saturating_duration_from_secs(
                size * bandwidth_divisor / bandwidth,
            ));
        match episode {
            Some(DiskDegradationState {
                kind: DiskEpisodeKind::Stall,
                expires_at,
            }) => steady.saturating_add(expires_at.saturating_sub(now)),
            _ => steady,
        }
    }
}

fn saturating_duration_from_secs(seconds: f64) -> Duration {
    if !seconds.is_finite() || seconds >= Duration::MAX.as_secs_f64() {
        Duration::MAX
    } else {
        Duration::from_secs_f64(seconds.max(0.0))
    }
}

impl Default for StorageEngine {
    fn default() -> Self {
        Self::new(StorageConfiguration::default())
    }
}
