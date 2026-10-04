//! State owned exclusively by the simulated storage engine.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    sync::Arc,
    time::Duration,
};

use moonpool_core::{IoConstraints, OpenOptions};

use super::OperationId;
use crate::storage::{
    FaultFocus, FileCrashReport, FileImage, StorageConfiguration, StorageFaultRecord,
    faults::EligibilitySlot,
    replication::{LayoutIndex, ReplicationPlan},
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
    /// Visible directories, excluding the implicit relative and absolute roots.
    pub(crate) directories: BTreeSet<Name>,
    /// Directory entries made durable by syncing their parent directory.
    pub(crate) durable_directories: BTreeSet<Name>,
    pub(crate) pending_ops: BTreeMap<OperationId, PendingStorageOp>,
    /// Consulted before any random fault damages a sector (see
    /// [`StorageEligibilityMask`]).
    pub(crate) eligibility: EligibilitySlot,
    /// Per-process fault weights (see [`FaultFocus`]); a process without one
    /// weighs every sector 1.
    pub(crate) focus: BTreeMap<IpAddr, Arc<FaultFocus>>,
    /// The seed's replicated fault patterns, one per covered group.
    pub(crate) replication: Vec<ReplicationSlot>,
    /// Per-process published layouts, which the pattern reads.
    pub(crate) layouts: BTreeMap<IpAddr, Arc<LayoutIndex>>,
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
            directories: BTreeSet::new(),
            durable_directories: BTreeSet::new(),
            pending_ops: BTreeMap::new(),
            eligibility: EligibilitySlot::default(),
            focus: BTreeMap::new(),
            replication: Vec::new(),
            layouts: BTreeMap::new(),
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

    /// How much more likely than configured a random fault is to damage
    /// `sector` of `owner`'s file at `path`: 0 where the eligibility mask
    /// vetoes it or the seed's replicated fault pattern spares it, else the
    /// owner's [`FaultFocus`] weight (1 without one).
    pub(crate) fn weight(&self, owner: IpAddr, path: &str, sector: u64) -> f64 {
        self.weigher(owner, path)(sector)
    }

    /// [`weight`](Self::weight) for one file, detached from the state so a
    /// file image can consult it while the engine holds the image mutably.
    pub(crate) fn weigher(&self, owner: IpAddr, path: &str) -> impl Fn(u64) -> f64 + use<> {
        let mask = self.eligibility.mask();
        let focus = self.focus.get(&owner).cloned();
        let replication: Vec<(Arc<ReplicationPlan>, usize)> = self
            .replication
            .iter()
            .map(|slot| (Arc::clone(&slot.plan), slot.turn))
            .collect();
        let layout = self.layouts.get(&owner).cloned();
        let path = path.to_string();
        move |sector| {
            if !mask.as_ref().is_none_or(|mask| mask(&path, sector)) {
                return 0.0;
            }
            if !replication
                .iter()
                .all(|(plan, turn)| plan.allows(owner, layout.as_deref(), &path, sector, *turn))
            {
                return 0.0;
            }
            focus
                .as_ref()
                .map_or(1.0, |focus| focus.weight(&path, sector))
        }
    }

    /// A random fault damaged or a write repaired some sector, or what a
    /// process publishes changed: each rolling turn re-reads its window's
    /// disks before it next moves.
    pub(crate) fn note_damage_changed(&mut self) {
        for slot in &mut self.replication {
            slot.recheck = true;
        }
    }

    /// Before `owner`'s disk is up for a fault: every rolling turn that
    /// covers `owner` from another domain, and whose window holds no
    /// damage, moves one domain on.
    pub(crate) fn settle_turns(&mut self, owner: IpAddr) {
        for at in 0..self.replication.len() {
            let plan = Arc::clone(&self.replication[at].plan);
            if !plan.is_rolling() || plan.tolerance() == 0 {
                // A window of no domain never holds the turn.
                continue;
            }
            let Some(domain) = plan.domain(owner) else {
                continue;
            };
            let turn = self.replication[at].turn;
            if self.replication[at].recheck {
                let damaged = self.window_damaged(&plan, turn);
                let slot = &mut self.replication[at];
                slot.damaged = damaged;
                slot.recheck = false;
            }
            let slot = &mut self.replication[at];
            if !slot.damaged && !plan.in_window(domain, turn) {
                slot.turn = (turn + 1) % plan.domain_count();
                slot.recheck = true;
                crate::assert_reachable!("storage: a rolling turn moves on");
            }
        }
    }

    /// Whether any named file of a process in the window starting at
    /// `turn` holds a damaged sector its process still publishes (or any
    /// damaged sector, if it publishes nothing).
    fn window_damaged(&self, plan: &ReplicationPlan, turn: usize) -> bool {
        self.path_to_file.iter().any(|((owner, path), file_id)| {
            if !plan
                .domain(*owner)
                .is_some_and(|domain| plan.in_window(domain, turn))
            {
                return false;
            }
            let Some(file) = self.files.get(file_id) else {
                return false;
            };
            if !file.image.has_damage() {
                return false;
            }
            let layout = self.layouts.get(owner);
            file.image
                .damaged_sectors()
                .into_iter()
                .any(|sector| layout.is_none_or(|layout| layout.covers(path, sector)))
        })
    }

    /// The weight of a fault rolled once for a whole range: 0 if any sector
    /// is vetoed or immune, else the heaviest sector's weight.
    pub(crate) fn weight_range(
        &self,
        owner: IpAddr,
        path: &str,
        sectors: std::ops::Range<u64>,
    ) -> f64 {
        let mut heaviest = 0.0_f64;
        for sector in sectors {
            let weight = self.weight(owner, path, sector);
            if weight <= 0.0 {
                return 0.0;
            }
            heaviest = heaviest.max(weight);
        }
        heaviest
    }
}

/// One group's replicated fault plan, and its rolling turn.
#[derive(Debug)]
pub(crate) struct ReplicationSlot {
    pub(crate) plan: Arc<ReplicationPlan>,
    /// Where the rolling window starts (unused by the other patterns).
    pub(crate) turn: usize,
    /// Whether the window held damage when last read.
    pub(crate) damaged: bool,
    /// Whether the window must be re-read before the turn next moves.
    pub(crate) recheck: bool,
}

impl ReplicationSlot {
    pub(crate) fn new(plan: ReplicationPlan) -> Self {
        Self {
            plan: Arc::new(plan),
            turn: 0,
            damaged: false,
            recheck: false,
        }
    }
}
