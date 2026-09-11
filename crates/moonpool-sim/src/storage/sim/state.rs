//! State owned exclusively by the simulated storage engine.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    time::Duration,
};

use moonpool_core::{IoConstraints, OpenOptions};

use super::OperationId;
use crate::storage::{
    FileCrashReport, FileImage, StorageConfiguration, StorageFaultRecord, faults::EligibilitySlot,
};

/// Unique identifier for persistent simulated file contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u64);

/// Unique identifier for one open simulated file handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HandleId(pub u64);

/// Kind of dynamic disk-degradation episode affecting a process disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskEpisodeKind {
    /// Disk is frozen until the episode expires.
    Stall,
    /// Effective IOPS and bandwidth are reduced.
    Throttle,
}

/// Active disk-degradation episode for a process disk.
#[derive(Debug, Clone, Copy)]
pub struct DiskDegradationState {
    /// Which kind of degradation is active.
    pub kind: DiskEpisodeKind,
    /// When the episode expires.
    pub expires_at: Duration,
}

#[derive(Debug)]
pub(crate) struct FileState {
    pub(crate) path: String,
    pub(crate) image: FileImage,
    pub(crate) owner_ip: IpAddr,
}

#[derive(Debug)]
pub(crate) struct HandleState {
    pub(crate) file_id: FileId,
    pub(crate) position: u64,
    pub(crate) options: OpenOptions,
    /// Alignment this handle enforces, fixed when the file was opened.
    pub(crate) constraints: IoConstraints,
    /// Whether the open actually got direct I/O.
    pub(crate) direct_io: bool,
    pub(crate) pending_ops: BTreeSet<OperationId>,
    pub(crate) is_closed: bool,
}

impl HandleState {
    pub(crate) fn new(
        file_id: FileId,
        position: u64,
        options: OpenOptions,
        constraints: IoConstraints,
        direct_io: bool,
    ) -> Self {
        Self {
            file_id,
            position,
            options,
            constraints,
            direct_io,
            pending_ops: BTreeSet::new(),
            is_closed: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingOpType {
    Read,
    Write,
    Sync,
    SetLen,
}

#[derive(Debug)]
pub(crate) struct PendingStorageOp {
    pub(crate) handle_id: HandleId,
    pub(crate) file_id: FileId,
    pub(crate) op_type: PendingOpType,
    pub(crate) offset: u64,
    pub(crate) len: usize,
    pub(crate) data: Option<Vec<u8>>,
    pub(crate) append: bool,
    /// A positioned operation (`read_at` / `write_at`): it addresses `offset`
    /// literally and never moves the handle's stream cursor.
    pub(crate) positioned: bool,
}

/// A name in a process's namespace: the owning process and the path.
pub(crate) type Name = (IpAddr, String);

/// Mutable storage data owned by [`super::StorageEngine`].
#[derive(Debug)]
pub(crate) struct StorageState {
    pub(crate) next_file_id: u64,
    pub(crate) next_handle_id: u64,
    pub(crate) next_operation_id: u64,
    pub(crate) config: StorageConfiguration,
    pub(crate) per_process_configs: BTreeMap<IpAddr, StorageConfiguration>,
    pub(crate) disk_episodes: BTreeMap<IpAddr, DiskDegradationState>,
    /// Disks that failed: every later operation on them parks forever. Only a
    /// crash or wipe of the owning process removes an entry.
    pub(crate) failed_disks: BTreeSet<IpAddr>,
    pub(crate) files: BTreeMap<FileId, FileState>,
    pub(crate) handles: BTreeMap<HandleId, HandleState>,
    /// The namespace as it is now, one per process: a name is a path *on a
    /// process's disk*, so two processes resolving the same relative path
    /// reach two files, as two machines would.
    pub(crate) path_to_file: BTreeMap<Name, FileId>,
    /// The namespace as it would survive a crash: entries promoted here by a
    /// directory sync. A create, delete, or rename changes `path_to_file`
    /// immediately and reaches this map only when the directory holding it is
    /// synced — file data durability and directory-entry durability are two
    /// different things.
    pub(crate) durable_paths: BTreeMap<Name, FileId>,
    pub(crate) pending_ops: BTreeMap<OperationId, PendingStorageOp>,
    /// Consulted before any random fault damages a sector (see
    /// [`StorageEligibilityMask`]).
    pub(crate) eligibility: EligibilitySlot,
    /// Every fault the disk has injected, oldest first, drained by the caller.
    pub(crate) fault_records: Vec<StorageFaultRecord>,
    /// What each simulated crash did, per file, drained by the caller.
    pub(crate) crash_reports: Vec<FileCrashReport>,
    /// Whether the barrier-violation family was ever armed.
    ///
    /// Latched rather than read back off the configuration, so disabling fault
    /// injection cannot turn a sector a pre-cutoff sync already lied about
    /// into an impossible-state panic in the crash oracle.
    pub(crate) barrier_violation_armed: bool,
}

impl StorageState {
    pub(crate) fn new(config: StorageConfiguration) -> Self {
        Self {
            next_file_id: 0,
            next_handle_id: 0,
            next_operation_id: 0,
            config,
            per_process_configs: BTreeMap::new(),
            disk_episodes: BTreeMap::new(),
            failed_disks: BTreeSet::new(),
            files: BTreeMap::new(),
            handles: BTreeMap::new(),
            path_to_file: BTreeMap::new(),
            durable_paths: BTreeMap::new(),
            pending_ops: BTreeMap::new(),
            eligibility: EligibilitySlot::default(),
            fault_records: Vec::new(),
            crash_reports: Vec::new(),
            barrier_violation_armed: false,
        }
    }

    pub(crate) fn config_for(&self, ip: IpAddr) -> &StorageConfiguration {
        self.per_process_configs.get(&ip).unwrap_or(&self.config)
    }

    /// Record one injected fault for the caller to inspect.
    pub(crate) fn record_fault(&mut self, record: StorageFaultRecord) {
        self.fault_records.push(record);
    }

    /// Whether a random fault may damage this sector.
    pub(crate) fn eligible(&self, path: &str, sector: u64) -> bool {
        self.eligibility.allows(path, sector)
    }

    /// Whether a random fault may damage every sector in the range.
    pub(crate) fn eligible_range(&self, path: &str, sectors: std::ops::Range<u64>) -> bool {
        sectors
            .into_iter()
            .all(|sector| self.eligible(path, sector))
    }
}
