//! Storage state transitions and fault injection.

use std::{
    collections::BTreeMap,
    net::IpAddr,
    ops::Range,
    task::{Poll, Waker},
    time::Duration,
};

use moonpool_core::{DirectIo, IoConstraints, OpenOptions};

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
    storage::{
        EioTarget, FileCrashReport, FileImage, StorageConfiguration, StorageEligibilityMask,
        StorageError, StorageFaultKind, StorageFaultRecord, StorageOperation, faults::sector_range,
    },
};

/// What the disk decided to do with one write.
#[derive(Debug, Clone, Copy)]
enum WriteLanding {
    /// The device refused it.
    Eio,
    /// It was acknowledged and the bytes never reached the disk.
    Phantom,
    /// It landed at this offset — the requested one, or another entirely.
    At(u64),
}

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
    /// waited out. A disk that already failed stays in `failed_disks`: it has
    /// no expiry, and only a crash or wipe of its process replaces it.
    pub(crate) fn disable_fault_injection(&mut self) {
        self.state.config.disable_fault_injection();
        for config in self.state.per_process_configs.values_mut() {
            config.disable_fault_injection();
        }
    }

    pub(crate) fn disk_episode_for(&self, ip: IpAddr) -> Option<DiskDegradationState> {
        self.state.disk_episodes.get(&ip).copied()
    }

    pub(crate) fn is_disk_failed(&self, ip: IpAddr) -> bool {
        self.state.failed_disks.contains(&ip)
    }

    /// Fail `ip`'s disk outright: every later operation on a file it owns is
    /// parked forever. A scripted injection, so it ignores the one-at-a-time
    /// budget the per-operation coin honors and consumes no randomness.
    pub(crate) fn fail_disk(&mut self, ip: IpAddr) -> StorageActions {
        let mut actions = StorageActions::default();
        if self.state.failed_disks.insert(ip) {
            actions.fault(SimFaultEvent::StorageDiskFailure { ip: ip.to_string() });
        }
        actions
    }

    pub(crate) fn open_file(
        &mut self,
        path: &str,
        options: OpenOptions,
        initial_size: u64,
        owner_ip: IpAddr,
    ) -> Result<HandleId, StorageError> {
        let path = path.to_string();
        let (constraints, direct_io) = self.resolve_direct_io(&options, owner_ip)?;
        if options.is_create_new() && self.state.path_to_file.contains_key(&path) {
            return Err(StorageError::AlreadyExists { path });
        }

        let file_id = if let Some(existing_id) = self.state.path_to_file.get(&path).copied() {
            if options.is_truncate()
                && let Some(file) = self.state.files.get_mut(&existing_id)
            {
                file.image.set_len(0);
            }
            existing_id
        } else {
            if !options.is_create() && !options.is_create_new() {
                return Err(StorageError::NotFound { path });
            }
            let file_id = FileId(self.state.next_file_id);
            self.state.next_file_id += 1;
            let garbage_fill =
                sim_random::<f64>() < self.state.config_for(owner_ip).garbage_fill_probability;
            if garbage_fill {
                assert_reachable!("disk: file fills never-written sectors with garbage");
            } else {
                assert_reachable!("disk: file fills never-written sectors with zeros");
            }
            self.state.files.insert(
                file_id,
                FileState {
                    path: path.clone(),
                    image: FileImage::new(initial_size, sim_random::<u64>(), garbage_fill),
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
                .map_or(0, |file| file.image.size())
        } else {
            0
        };
        let handle_id = HandleId(self.state.next_handle_id);
        self.state.next_handle_id += 1;
        self.state.handles.insert(
            handle_id,
            HandleState::new(file_id, position, options, constraints, direct_io),
        );
        Ok(handle_id)
    }

    /// Resolve an open's direct-I/O policy against the disk's geometry.
    ///
    /// `Required` never silently downgrades: a disk that cannot provide
    /// uncached I/O fails the open. `Optional` falls back to buffered I/O and
    /// says so through the handle's reported constraints. Neither draws
    /// randomness — this is device geometry, not a fault.
    fn resolve_direct_io(
        &self,
        options: &OpenOptions,
        owner_ip: IpAddr,
    ) -> Result<(IoConstraints, bool), StorageError> {
        let config = self.state.config_for(owner_ip);
        let supported = config.direct_io_supported;
        let alignment = config.direct_io_alignment;
        let honored = match options.requested_direct_io() {
            DirectIo::Disabled => false,
            DirectIo::Optional => supported,
            DirectIo::Required if supported => true,
            DirectIo::Required => return Err(StorageError::DirectIoUnsupported),
        };
        Ok(if honored {
            (IoConstraints::uniform(alignment), true)
        } else {
            (IoConstraints::NONE, false)
        })
    }

    /// The alignment `handle_id` enforces on every transfer.
    pub(crate) fn handle_constraints(
        &self,
        handle_id: HandleId,
    ) -> Result<IoConstraints, StorageError> {
        Ok(self.open_handle(handle_id)?.constraints)
    }

    /// Whether `handle_id` is doing direct (uncached) I/O.
    pub(crate) fn handle_is_direct_io(&self, handle_id: HandleId) -> Result<bool, StorageError> {
        Ok(self.open_handle(handle_id)?.direct_io)
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
        self.drop_file_if_unlinked(file_id);
        let mut actions = StorageActions::default();
        self.invalidate_file_handles(file_id, &mut actions, false);
        Ok(actions)
    }

    /// Forget a file's contents once no name — visible or durable — reaches
    /// it any more. A file whose only remaining link is the durable one is
    /// still on the disk: a crash before the directory sync brings it back.
    fn drop_file_if_unlinked(&mut self, file_id: FileId) {
        let linked = self
            .state
            .path_to_file
            .values()
            .chain(self.state.durable_paths.values())
            .any(|id| *id == file_id);
        if !linked {
            self.state.files.remove(&file_id);
        }
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
            self.drop_file_if_unlinked(replaced_id);
            self.invalidate_file_handles(replaced_id, &mut actions, false);
        }
        if let Some(file) = self.state.files.get_mut(&file_id) {
            file.path = to.to_string();
        }
        self.state.path_to_file.insert(to.to_string(), file_id);
        Ok(actions)
    }

    /// Install the eligibility mask consulted before any random fault damages
    /// a sector.
    pub(crate) fn set_eligibility_mask(&mut self, mask: Option<StorageEligibilityMask>) {
        self.state.eligibility.set(mask);
    }

    /// Drain the faults injected so far.
    pub(crate) fn take_fault_records(&mut self) -> Vec<StorageFaultRecord> {
        std::mem::take(&mut self.state.fault_records)
    }

    /// Drain the per-file reports of the crashes simulated so far.
    pub(crate) fn take_crash_reports(&mut self) -> Vec<FileCrashReport> {
        std::mem::take(&mut self.state.crash_reports)
    }

    /// Plant a latent read fault on `sectors` of `path`.
    pub(crate) fn corrupt_file(
        &mut self,
        path: &str,
        sectors: Range<u64>,
    ) -> Result<(), StorageError> {
        self.file_mut(path)?.image.corrupt(sectors);
        Ok(())
    }

    /// Make reads and/or writes touching `sectors` of `path` fail with EIO.
    pub(crate) fn fail_file_with_eio(
        &mut self,
        path: &str,
        sectors: Range<u64>,
        target: EioTarget,
    ) -> Result<(), StorageError> {
        self.file_mut(path)?.image.fail_with_eio(sectors, target);
        Ok(())
    }

    /// Clear targeted EIO injections on `path`.
    pub(crate) fn clear_file_eio(
        &mut self,
        path: &str,
        target: EioTarget,
    ) -> Result<(), StorageError> {
        self.file_mut(path)?.image.clear_eio(target);
        Ok(())
    }

    /// Mutate a durable sector of `path` out of band, bypassing the crash
    /// model: a deliberate simulator bug, so the crash oracle can be tested.
    pub(crate) fn corrupt_durable_out_of_band(
        &mut self,
        path: &str,
        sector: u64,
    ) -> Result<(), StorageError> {
        self.file_mut(path)?
            .image
            .corrupt_committed_out_of_band(sector);
        Ok(())
    }

    fn file_mut(&mut self, path: &str) -> Result<&mut FileState, StorageError> {
        let file_id =
            self.state
                .path_to_file
                .get(path)
                .copied()
                .ok_or_else(|| StorageError::NotFound {
                    path: path.to_string(),
                })?;
        self.state
            .files
            .get_mut(&file_id)
            .ok_or(StorageError::MissingFile { file_id })
    }

    /// Make every directory entry directly under `path` durable.
    ///
    /// The simulated namespace is a flat map from path to file, so a
    /// "directory" is a path prefix: this promotes exactly the entries whose
    /// parent is `path`, in both directions — names created since the last
    /// sync become durable, names deleted or renamed away stop being durable.
    /// A directory with no entries syncs successfully and changes nothing.
    ///
    /// Namespace operations complete without a scheduled event, like `delete`
    /// and `rename`, so this one does too.
    pub(crate) fn sync_dir(
        &mut self,
        path: &str,
        owner_ip: IpAddr,
    ) -> Result<StorageActions, StorageError> {
        let mut actions = StorageActions::default();
        let probability = self.state.config_for(owner_ip).sync_failure_probability;
        if probability > 0.0 && sim_random::<f64>() < probability {
            assert_reachable!("disk: directory sync failed");
            self.record(path, StorageFaultKind::SyncFailure, None);
            actions.fault(SimFaultEvent::StorageSyncFault {
                ip: owner_ip.to_string(),
                file_id: u64::MAX,
            });
            return Err(StorageError::Io {
                file_id: FileId(u64::MAX),
                kind: std::io::ErrorKind::Other,
                message: "directory sync failed (simulated I/O error)".to_string(),
            });
        }

        let directory = normalize_directory(path);
        self.state
            .durable_paths
            .retain(|entry, _| parent_directory(entry) != directory);
        for (entry, file_id) in &self.state.path_to_file {
            if parent_directory(entry) == directory {
                self.state.durable_paths.insert(entry.clone(), *file_id);
            }
        }
        Ok(actions)
    }

    /// Resolve the namespace a crash leaves behind.
    ///
    /// Every divergence between the visible namespace and the durable one is
    /// an unsynced directory operation, and each is resolved independently: a
    /// name created since the last directory sync may not be there, and a name
    /// deleted or renamed away since then may still be. Contents are dropped
    /// once no surviving name reaches them.
    ///
    /// The coin is drawn only while `unsynced_dir_entry_loss_probability` is
    /// positive, so a configuration with the family off consumes no
    /// randomness — and then the visible namespace survives whole, which is
    /// what every test that does not care about entry durability expects.
    fn resolve_namespace_crash(&mut self, ip: IpAddr) {
        let probability = self
            .state
            .config_for(ip)
            .unsynced_dir_entry_loss_probability;
        if probability > 0.0 {
            self.lose_unsynced_entries(ip, probability);
        }

        // Whatever survived is what is on the disk now — for this process's
        // files only. Another process's unsynced entries are not made durable
        // by this crash.
        let mine = self.files_owned_by(ip);
        self.state
            .durable_paths
            .retain(|_, file_id| !mine.contains(file_id));
        let surviving: Vec<(String, FileId)> = self
            .state
            .path_to_file
            .iter()
            .filter(|(_, file_id)| mine.contains(file_id))
            .map(|(path, file_id)| (path.clone(), *file_id))
            .collect();
        for (path, file_id) in surviving {
            self.state.durable_paths.insert(path, file_id);
        }

        // Contents no surviving name reaches are gone with the name.
        let linked: Vec<FileId> = self
            .state
            .path_to_file
            .values()
            .chain(self.state.durable_paths.values())
            .copied()
            .collect();
        self.state
            .files
            .retain(|file_id, file| file.owner_ip != ip || linked.contains(file_id));
    }

    /// Roll the loss coin for each of this process's unsynced directory
    /// operations: a name created since the last directory sync may not be
    /// there, and one deleted or renamed away may still be.
    fn lose_unsynced_entries(&mut self, ip: IpAddr, probability: f64) {
        let mine = self.files_owned_by(ip);
        let mut paths: Vec<String> = self
            .state
            .path_to_file
            .keys()
            .chain(self.state.durable_paths.keys())
            .cloned()
            .collect();
        paths.sort_unstable();
        paths.dedup();

        for path in paths {
            let visible = self.state.path_to_file.get(&path).copied();
            let durable = self.state.durable_paths.get(&path).copied();
            if visible == durable {
                continue;
            }
            // Only this process's own entries are at risk.
            if !visible
                .iter()
                .chain(durable.iter())
                .any(|file_id| mine.contains(file_id))
            {
                continue;
            }
            if sim_random::<f64>() >= probability {
                continue;
            }
            assert_reachable!("disk: crash lost an unsynced directory entry");
            self.state.record_fault(StorageFaultRecord {
                path: path.clone(),
                kind: StorageFaultKind::DirEntryLost,
                sectors: None,
            });
            match durable {
                Some(file_id) => self.state.path_to_file.insert(path, file_id),
                None => self.state.path_to_file.remove(&path),
            };
        }
    }

    /// The files this process owns, in stable order.
    fn files_owned_by(&self, ip: IpAddr) -> Vec<FileId> {
        self.state
            .files
            .iter()
            .filter_map(|(file_id, file)| (file.owner_ip == ip).then_some(*file_id))
            .collect()
    }

    pub(crate) fn schedule_read(
        &mut self,
        handle_id: HandleId,
        offset: u64,
        len: usize,
        positioned: bool,
        now: Duration,
    ) -> Result<(OperationId, StorageActions), StorageError> {
        self.ensure_readable(handle_id)?;
        let file_id = self.open_file_id(handle_id)?;
        let owner_ip = self.owner_ip(file_id)?;
        let len = self.shorten_transfer(owner_ip, len);
        let pending = PendingStorageOp {
            handle_id,
            file_id,
            op_type: PendingOpType::Read,
            offset,
            len,
            data: None,
            append: false,
            positioned,
        };
        if let Some(actions) = self.roll_disk_failure(owner_ip) {
            return self.park_operation(pending, actions);
        }
        let episode = self.update_disk_episode(owner_ip, now);
        let latency = Self::calculate_storage_latency(
            self.state.config_for(owner_ip),
            len,
            false,
            episode,
            now,
        );
        self.schedule_operation(
            pending,
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
        mut data: Vec<u8>,
        positioned: bool,
        now: Duration,
    ) -> Result<(OperationId, StorageActions), StorageError> {
        self.ensure_writable(handle_id, "write")?;
        let file_id = self.open_file_id(handle_id)?;
        // Append mode moves the stream cursor, never a positioned write.
        let append = !positioned && self.open_handle(handle_id)?.options.is_append();
        let owner_ip = self.owner_ip(file_id)?;
        data.truncate(self.shorten_transfer(owner_ip, data.len()));
        let len = data.len();
        let pending = PendingStorageOp {
            handle_id,
            file_id,
            op_type: PendingOpType::Write,
            offset,
            len,
            data: Some(data),
            append,
            positioned,
        };
        if let Some(actions) = self.roll_disk_failure(owner_ip) {
            return self.park_operation(pending, actions);
        }
        let episode = self.update_disk_episode(owner_ip, now);
        let latency = Self::calculate_storage_latency(
            self.state.config_for(owner_ip),
            len,
            true,
            episode,
            now,
        );
        self.schedule_operation(
            pending,
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
        let pending = PendingStorageOp {
            handle_id,
            file_id,
            op_type: PendingOpType::Sync,
            offset: 0,
            len: 0,
            data: None,
            append: false,
            positioned: false,
        };
        if let Some(actions) = self.roll_disk_failure(owner_ip) {
            return self.park_operation(pending, actions);
        }
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
            pending,
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
        let pending = PendingStorageOp {
            handle_id,
            file_id,
            op_type: PendingOpType::SetLen,
            offset: new_len,
            len: 0,
            data: None,
            append: false,
            positioned: false,
        };
        if let Some(actions) = self.roll_disk_failure(owner_ip) {
            return self.park_operation(pending, actions);
        }
        let latency = sample_latency(&self.state.config_for(owner_ip).write_latency);
        self.schedule_operation(
            pending,
            StorageOperation::SetLenComplete { new_len },
            now.saturating_add(latency),
        )
    }

    /// Shorten one transfer to a deterministic prefix, the way a real
    /// `read`/`write` may move fewer bytes than asked for.
    ///
    /// The coin is drawn only while `short_transfer_probability` is positive,
    /// so a configuration with the family off consumes no randomness. A
    /// one-byte transfer is left alone: the only shorter transfer is zero,
    /// which would mean EOF rather than a short read.
    fn shorten_transfer(&self, owner_ip: IpAddr, len: usize) -> usize {
        let probability = self.state.config_for(owner_ip).short_transfer_probability;
        if probability <= 0.0 || len <= 1 || sim_random::<f64>() >= probability {
            return len;
        }
        assert_reachable!("disk: transfer moved fewer bytes than requested");
        sim_random_range(1..len)
    }

    /// Whether `owner_ip`'s disk is failed, drawing the failure coin first if
    /// the disk is healthy and the budget allows. A failed disk answers with
    /// the actions to apply (the fault event, on the operation that failed
    /// it); a healthy one answers `None`.
    ///
    /// The coin is drawn only while `disk_failure_probability` is positive, so
    /// a configuration with the family off consumes no randomness here.
    fn roll_disk_failure(&mut self, owner_ip: IpAddr) -> Option<StorageActions> {
        if self.state.failed_disks.contains(&owner_ip) {
            return Some(StorageActions::default());
        }
        // Floor: one failed disk at a time. The coin never takes a second
        // member of a quorum system down while the first is still hung; a
        // crash or wipe of the first frees the budget.
        if !self.state.failed_disks.is_empty() {
            return None;
        }
        let probability = self.state.config_for(owner_ip).disk_failure_probability;
        if probability <= 0.0 || sim_random::<f64>() >= probability {
            return None;
        }
        assert_reachable!("disk: failed, every later I/O parks forever");
        Some(self.fail_disk(owner_ip))
    }

    /// Register `pending` as in flight without scheduling its completion.
    ///
    /// The operation stays `Pending` for good: nothing in the engine will ever
    /// resolve it. It leaves `pending_ops` only when its future is dropped
    /// (`cancel_operation`), when a crash or wipe of the owning process fails
    /// the handle's operations, or at simulation shutdown. It samples no
    /// latency and enters no episode, so a hung I/O draws nothing.
    fn park_operation(
        &mut self,
        pending: PendingStorageOp,
        actions: StorageActions,
    ) -> Result<(OperationId, StorageActions), StorageError> {
        let operation_id = self.register_operation(pending)?;
        Ok((operation_id, actions))
    }

    fn schedule_operation(
        &mut self,
        pending: PendingStorageOp,
        operation: StorageOperation,
        at: Duration,
    ) -> Result<(OperationId, StorageActions), StorageError> {
        let handle_id = pending.handle_id;
        let operation_id = self.register_operation(pending)?;
        let mut actions = StorageActions::default();
        actions.schedule(at, StorageEvent::new(operation_id, handle_id, operation));
        Ok((operation_id, actions))
    }

    fn register_operation(
        &mut self,
        pending: PendingStorageOp,
    ) -> Result<OperationId, StorageError> {
        let operation_id = OperationId(self.state.next_operation_id);
        self.state.next_operation_id += 1;
        let handle = self.open_handle_mut(pending.handle_id)?;
        handle.pending_ops.insert(operation_id);
        self.state.pending_ops.insert(operation_id, pending);
        Ok(operation_id)
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
        let (owner_ip, path, file_size, config) = self
            .state
            .files
            .get(&pending.file_id)
            .map(|file| {
                (
                    file.owner_ip,
                    file.path.clone(),
                    file.image.size(),
                    self.state.config_for(file.owner_ip).clone(),
                )
            })
            .ok_or(StorageError::InvalidFileHandle {
                handle_id: pending.handle_id,
            })?;
        let sectors = sector_range(pending.offset, pending.len);

        // EIO: a targeted injection fires unconditionally; the random family
        // is rolled first, then gated by the eligibility mask, so installing a
        // mask never shifts the stream.
        let eio_roll = sim_random::<f64>();
        let targeted = self
            .state
            .files
            .get(&pending.file_id)
            .is_some_and(|file| file.image.read_fails(sectors.clone()));
        let random_hit =
            config.read_fault_eio_probability > 0.0 && eio_roll < config.read_fault_eio_probability;
        if targeted || (random_hit && self.state.eligible_range(&path, sectors.clone())) {
            assert_reachable!("disk fault: read failed with EIO");
            self.record(&path, StorageFaultKind::EioRead, Some(sectors));
            actions.fault(SimFaultEvent::StorageReadFault {
                ip: owner_ip.to_string(),
                file_id: pending.file_id.0,
            });
            return Err(StorageError::Io {
                file_id: pending.file_id,
                kind: std::io::ErrorKind::Other,
                message: "read failed (simulated I/O error)".to_string(),
            });
        }

        // Read-time latent corruption: the sector is damaged from now on, and
        // damaged identically on every retry.
        let mut read_faulted = false;
        if config.read_fault_probability > 0.0 {
            for sector in sectors.clone() {
                let hit = sim_random::<f64>() < config.read_fault_probability;
                if hit && self.state.eligible(&path, sector) {
                    if let Some(file) = self.state.files.get_mut(&pending.file_id) {
                        file.image.corrupt(sector..sector + 1);
                    }
                    read_faulted = true;
                }
            }
        }
        if read_faulted {
            assert_reachable!("disk fault: read planted latent corruption");
            self.record(&path, StorageFaultKind::ReadCorruption, Some(sectors));
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
        if misdirected {
            assert_reachable!("disk fault: read served from the wrong offset");
            self.record(
                &path,
                StorageFaultKind::MisdirectedRead,
                Some(sector_range(read_offset, pending.len)),
            );
        }

        let mut data = vec![0; pending.len];
        let file =
            self.state
                .files
                .get(&pending.file_id)
                .ok_or(StorageError::InvalidFileHandle {
                    handle_id: pending.handle_id,
                })?;
        file.image
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

    /// Where one write actually lands, once the disk has had its say.
    fn complete_write(
        &mut self,
        operation_id: OperationId,
        pending: PendingStorageOp,
        actions: &mut StorageActions,
    ) -> Result<StorageCompletion, StorageError> {
        let Some(data) = pending.data else {
            return Err(StorageError::InvalidOperationData { operation_id });
        };
        let (owner_ip, path, config) = self
            .state
            .files
            .get(&pending.file_id)
            .map(|file| {
                (
                    file.owner_ip,
                    file.path.clone(),
                    self.state.config_for(file.owner_ip).clone(),
                )
            })
            .ok_or(StorageError::InvalidFileHandle {
                handle_id: pending.handle_id,
            })?;
        let file_size = self
            .state
            .files
            .get(&pending.file_id)
            .map_or(0, |file| file.image.size());
        // Append mode is the stream cursor's business: a positioned write was
        // already told exactly where it goes.
        let offset = if pending.append {
            file_size
        } else {
            pending.offset
        };

        let landing = self.decide_write_landing(&path, offset, data.len(), file_size, &config);
        let mut fault_kind = None;
        match landing {
            WriteLanding::Eio => {
                self.record(
                    &path,
                    StorageFaultKind::EioWrite,
                    Some(sector_range(offset, data.len())),
                );
                actions.fault(SimFaultEvent::StorageWriteFault {
                    ip: owner_ip.to_string(),
                    file_id: pending.file_id.0,
                    write_kind: "eio".to_string(),
                });
                return Err(StorageError::Io {
                    file_id: pending.file_id,
                    kind: std::io::ErrorKind::Other,
                    message: "write failed (simulated I/O error)".to_string(),
                });
            }
            WriteLanding::Phantom => {
                // Acknowledged, never applied: the bytes never reach the disk
                // and a reader keeps seeing the old contents.
                self.record(
                    &path,
                    StorageFaultKind::PhantomWrite,
                    Some(sector_range(offset, data.len())),
                );
                fault_kind = Some("phantom");
            }
            WriteLanding::At(landed_at) => {
                if landed_at != offset {
                    self.record(
                        &path,
                        StorageFaultKind::MisdirectedWrite,
                        Some(sector_range(landed_at, data.len())),
                    );
                    fault_kind = Some("misdirected");
                }
                let Some(file) = self.state.files.get_mut(&pending.file_id) else {
                    return Err(StorageError::InvalidFileHandle {
                        handle_id: pending.handle_id,
                    });
                };
                file.image.write(landed_at, &data);
                if self.plant_write_corruption(
                    pending.file_id,
                    &path,
                    landed_at,
                    data.len(),
                    &config,
                ) {
                    self.record(
                        &path,
                        StorageFaultKind::WriteCorruption,
                        Some(sector_range(landed_at, data.len())),
                    );
                    fault_kind = Some("corruption");
                }
            }
        }

        if let Some(write_kind) = fault_kind {
            actions.fault(SimFaultEvent::StorageWriteFault {
                ip: owner_ip.to_string(),
                file_id: pending.file_id.0,
                write_kind: write_kind.to_string(),
            });
        }
        let len = data.len();
        if !pending.positioned
            && let Some(handle) = self.state.handles.get_mut(&pending.handle_id)
        {
            handle.position = offset + len as u64;
        }
        Ok(StorageCompletion::Write { offset, len })
    }

    /// Decide what the disk does with one write: refuse it, swallow it, or
    /// land it — here, or somewhere else entirely.
    ///
    /// Every roll happens before the eligibility mask is consulted, so
    /// installing a mask never shifts the random stream.
    fn decide_write_landing(
        &mut self,
        path: &str,
        offset: u64,
        len: usize,
        file_size: u64,
        config: &StorageConfiguration,
    ) -> WriteLanding {
        let sectors = sector_range(offset, len);
        let eio_roll = sim_random::<f64>();
        let targeted = self
            .image_at(path)
            .is_some_and(|image| image.write_fails(sectors.clone()));
        let random_eio = config.write_fault_eio_probability > 0.0
            && eio_roll < config.write_fault_eio_probability;
        if targeted || (random_eio && self.state.eligible_range(path, sectors.clone())) {
            assert_reachable!("disk fault: write failed with EIO");
            return WriteLanding::Eio;
        }

        let phantom_roll = sim_random::<f64>();
        let misdirect_roll = sim_random::<f64>();
        if config.phantom_write_probability > 0.0
            && phantom_roll < config.phantom_write_probability
            && self.state.eligible_range(path, sectors.clone())
        {
            assert_reachable!("disk fault: phantom write dropped");
            return WriteLanding::Phantom;
        }

        if config.misdirect_write_probability > 0.0
            && misdirect_roll < config.misdirect_write_probability
        {
            let max_offset = file_size.saturating_sub(len as u64);
            let mistaken = if max_offset > 0 {
                sim_random_range(0..max_offset)
            } else {
                0
            };
            let mistaken_sectors = sector_range(mistaken, len);
            if mistaken != offset
                && self.state.eligible_range(path, sectors)
                && self.state.eligible_range(path, mistaken_sectors)
            {
                assert_reachable!("disk fault: misdirected write landed elsewhere");
                return WriteLanding::At(mistaken);
            }
        }
        WriteLanding::At(offset)
    }

    /// Roll write-time latent corruption over the sectors a write touched.
    /// Returns whether any sector was damaged.
    fn plant_write_corruption(
        &mut self,
        file_id: FileId,
        path: &str,
        offset: u64,
        len: usize,
        config: &StorageConfiguration,
    ) -> bool {
        if config.write_fault_probability <= 0.0 {
            return false;
        }
        let mut corrupted = false;
        for sector in sector_range(offset, len) {
            let hit = sim_random::<f64>() < config.write_fault_probability;
            if hit && self.state.eligible(path, sector) {
                if let Some(file) = self.state.files.get_mut(&file_id) {
                    file.image.corrupt(sector..sector + 1);
                }
                corrupted = true;
            }
        }
        if corrupted {
            assert_reachable!("disk fault: write planted latent corruption");
        }
        corrupted
    }

    /// The image behind a path, if the path still names a file.
    fn image_at(&self, path: &str) -> Option<&FileImage> {
        let file_id = self.state.path_to_file.get(path)?;
        self.state.files.get(file_id).map(|file| &file.image)
    }

    fn complete_sync(
        &mut self,
        pending: &PendingStorageOp,
        actions: &mut StorageActions,
    ) -> Result<(), StorageError> {
        let Some((owner_ip, path)) = self
            .state
            .files
            .get(&pending.file_id)
            .map(|file| (file.owner_ip, file.path.clone()))
        else {
            return Err(StorageError::InvalidFileHandle {
                handle_id: pending.handle_id,
            });
        };
        let config = self.state.config_for(owner_ip).clone();
        if config.sync_failure_probability > 0.0
            && sim_random::<f64>() < config.sync_failure_probability
        {
            assert_reachable!("disk fault: sync failed");
            self.record(&path, StorageFaultKind::SyncFailure, None);
            actions.fault(SimFaultEvent::StorageSyncFault {
                ip: owner_ip.to_string(),
                file_id: pending.file_id.0,
            });
            return Err(StorageError::Io {
                file_id: pending.file_id,
                kind: std::io::ErrorKind::Other,
                message: "sync failed (simulated I/O error)".to_string(),
            });
        }

        // A lying sync reports a sector durable while leaving it volatile;
        // the crash oracle later reports what that lie cost.
        self.state.barrier_violation_armed |= config.barrier_violation_probability > 0.0;
        let eligibility = self.state.eligibility.mask();
        let eligible =
            move |sector: u64| eligibility.as_ref().is_none_or(|mask| mask(&path, sector));
        let Some(file) = self.state.files.get_mut(&pending.file_id) else {
            return Err(StorageError::InvalidFileHandle {
                handle_id: pending.handle_id,
            });
        };
        file.image.sync(&config, &eligible);
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
        file.image.set_len(new_len);
        Ok(())
    }

    /// Record one injected fault against a path.
    fn record(&mut self, path: &str, kind: StorageFaultKind, sectors: Option<Range<u64>>) {
        self.state.record_fault(StorageFaultRecord {
            path: path.to_string(),
            kind,
            sectors,
        });
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
            .map(|file| file.image.size())
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
        // The reboot replaces a failed disk; the operations it parked are
        // failed below with the rest of the process's in-flight I/O.
        self.state.failed_disks.remove(&ip);
        let config = self.state.config_for(ip).clone();
        let armed = self.state.barrier_violation_armed;
        let file_ids = self.files_owned_by(ip);
        // Resolve each file's unsynced writes through the crash model, in
        // stable file order.
        for file_id in &file_ids {
            let Some(path) = self.state.files.get(file_id).map(|file| file.path.clone()) else {
                continue;
            };
            let eligibility = self.state.eligibility.mask();
            let eligible = {
                let path = path.clone();
                move |sector: u64| eligibility.as_ref().is_none_or(|mask| mask(&path, sector))
            };
            let Some(file) = self.state.files.get_mut(file_id) else {
                continue;
            };
            let report = file.image.crash(&path, &config, armed, &eligible);
            for sector in &report.lost_synced {
                self.state.record_fault(StorageFaultRecord {
                    path: path.clone(),
                    kind: StorageFaultKind::LostSyncedWrite,
                    sectors: Some(*sector..*sector + 1),
                });
            }
            self.state.crash_reports.push(report);
        }

        self.resolve_namespace_crash(ip);

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
        self.state.failed_disks.remove(&ip);
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
            self.state.durable_paths.retain(|_, id| *id != file_id);
            let _ = path;
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

/// The directory part of a path: everything before the last separator, or the
/// root (an empty string) for a name with no separator at all.
fn parent_directory(path: &str) -> &str {
    match path.rfind('/') {
        Some(index) => &path[..index],
        None => "",
    }
}

/// Normalize a directory path for comparison against [`parent_directory`]:
/// `"db/"`, `"db"` and (for the root) `"/"`, `"."`, `""` all name the same
/// directory.
fn normalize_directory(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed == "." { "" } else { trimmed }
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
