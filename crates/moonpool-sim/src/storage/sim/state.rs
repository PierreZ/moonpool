//! State owned exclusively by the simulated storage engine.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::IpAddr,
    time::Duration,
};

use moonpool_core::OpenOptions;

use super::OperationId;
use crate::storage::{InMemoryStorage, StorageConfiguration};

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
    pub(crate) storage: InMemoryStorage,
    pub(crate) owner_ip: IpAddr,
}

#[derive(Debug)]
pub(crate) struct HandleState {
    pub(crate) file_id: FileId,
    pub(crate) position: u64,
    pub(crate) options: OpenOptions,
    pub(crate) pending_ops: BTreeSet<OperationId>,
    pub(crate) is_closed: bool,
}

impl HandleState {
    pub(crate) fn new(file_id: FileId, position: u64, options: OpenOptions) -> Self {
        Self {
            file_id,
            position,
            options,
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
}

/// Mutable storage data owned by [`super::StorageEngine`].
#[derive(Debug)]
pub(crate) struct StorageState {
    pub(crate) next_file_id: u64,
    pub(crate) next_handle_id: u64,
    pub(crate) next_operation_id: u64,
    pub(crate) config: StorageConfiguration,
    pub(crate) per_process_configs: HashMap<IpAddr, StorageConfiguration>,
    pub(crate) disk_episodes: HashMap<IpAddr, DiskDegradationState>,
    pub(crate) files: BTreeMap<FileId, FileState>,
    pub(crate) handles: BTreeMap<HandleId, HandleState>,
    pub(crate) path_to_file: BTreeMap<String, FileId>,
    pub(crate) pending_ops: BTreeMap<OperationId, PendingStorageOp>,
}

impl StorageState {
    pub(crate) fn new(config: StorageConfiguration) -> Self {
        Self {
            next_file_id: 0,
            next_handle_id: 0,
            next_operation_id: 0,
            config,
            per_process_configs: HashMap::new(),
            disk_episodes: HashMap::new(),
            files: BTreeMap::new(),
            handles: BTreeMap::new(),
            path_to_file: BTreeMap::new(),
            pending_ops: BTreeMap::new(),
        }
    }

    pub(crate) fn config_for(&self, ip: IpAddr) -> &StorageConfiguration {
        self.per_process_configs.get(&ip).unwrap_or(&self.config)
    }
}
