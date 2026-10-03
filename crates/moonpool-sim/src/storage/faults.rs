//! Observable fault vocabulary of the simulated disk.
//!
//! Coordinates are **file plus flat sector offset**. There is no region, no
//! sub-file namespace, and no database-shaped boundary in the fault model: a
//! file is one byte range, and a fault names where in that range it happened.
//! What the bytes at that offset mean — a journal record, a page, a
//! superblock — belongs to the format written on top, never here.

use std::{ops::Range, sync::Arc};

/// Sector size of the simulated disk, in bytes.
///
/// The granularity of every sector-scoped fault: corruption, a crash
/// resolution, a targeted EIO. It is the *device's* unit, not the caller's:
/// a database page or a [`BlockFile`](moonpool_core::BlockFile) block is
/// typically several sectors, and neither is a crash-atomicity unit.
pub const SECTOR_SIZE: usize = 512;

/// Which operations a targeted EIO injection applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EioTarget {
    /// Fail reads touching the sectors.
    Read,
    /// Fail writes touching the sectors.
    Write,
    /// Fail both reads and writes touching the sectors.
    ReadWrite,
}

/// Fault family of one recorded storage fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageFaultKind {
    /// A read failed with a simulated I/O error.
    EioRead,
    /// A write failed with a simulated I/O error.
    EioWrite,
    /// A read planted a latent, deterministic sector corruption.
    ReadCorruption,
    /// A write planted a latent, deterministic sector corruption.
    WriteCorruption,
    /// A read was served from the wrong offset.
    MisdirectedRead,
    /// A write landed at the wrong offset in the file.
    MisdirectedWrite,
    /// A write completed but its bytes never reached the disk.
    PhantomWrite,
    /// A file or directory sync failed.
    SyncFailure,
    /// A crash lost a write that a sync had reported durable
    /// (barrier-violation family).
    LostSyncedWrite,
    /// A crash lost an unsynced directory entry.
    DirEntryLost,
}

/// One observable fault injected by the simulated disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageFaultRecord {
    /// Path of the affected file, or the directory for a namespace fault.
    pub path: String,
    /// Fault family.
    pub kind: StorageFaultKind,
    /// Affected sector range within the file, when the fault is sector-scoped.
    pub sectors: Option<Range<u64>>,
}

/// Resolution shape of one dirty sector during a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashOutcome {
    /// The sector reverted to its last durable contents.
    KeptOld,
    /// The unsynced write survived intact.
    KeptNew,
    /// The sector reverted to never-written and reads the disk's fill pattern
    /// (zeros or garbage, chosen per file).
    Lost,
    /// The unsynced write landed, but reads return deterministically corrupted
    /// bytes (identical on retry).
    LatentFault,
    /// A sub-sector prefix/suffix mix of old and new bytes (opt-in; weakens
    /// the sector-atomicity clause).
    Shorn,
}

/// Crash resolution of one sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectorResolution {
    /// Sector index within the file.
    pub sector: u64,
    /// How the sector resolved.
    pub outcome: CrashOutcome,
    /// Whether the sector was part of a correlated rollback run.
    pub correlated: bool,
}

/// What one crash did to one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileCrashReport {
    /// Path of the crashed file.
    pub path: String,
    /// The crash was fully clean: every unsynced write survived.
    pub clean: bool,
    /// Per-sector resolutions of the sectors dirty at crash time.
    pub resolutions: Vec<SectorResolution>,
    /// The file's unsynced length change was reverted.
    pub length_reverted: bool,
    /// Sectors a sync reported durable that the crash lost anyway — non-empty
    /// only with the barrier-violation family armed.
    pub lost_synced: Vec<u64>,
}

/// Eligibility mask consulted before injecting any random fault:
/// `(path, sector) -> eligible`.
///
/// A replication-aware harness can enforce "never damage all copies of one
/// record" without moonpool knowing what a replica is (`TigerBeetle`'s
/// `ClusterFaultAtlas` pattern). The mask gates the random fault families and
/// the damaging crash outcomes ([`CrashOutcome::Lost`],
/// [`CrashOutcome::LatentFault`], [`CrashOutcome::Shorn`]); an ineligible
/// sector falls back to the always-legal old/new resolution. Random draws are
/// made *before* the mask is consulted, so installing a mask never shifts the
/// RNG stream. The mask is consulted in stable (path, ascending sector) order.
pub type StorageEligibilityMask = Arc<dyn Fn(&str, u64) -> bool + Send + Sync>;

/// Where one process's random storage faults should land more, or less,
/// often: a weight per byte range of a file, and a background weight for
/// every other sector of that disk.
///
/// The simulator is layout-blind: a random fault family hits whatever
/// sector an operation touches, and most of a preallocated file is zeros
/// nobody reads. A harness that knows its layout — a journal's slot table,
/// its last batch, the two copies of a header — supplies the map, and the
/// simulator concentrates damage there. A focus **scales** the probability
/// of every family rolled per sector or per operation (read and write
/// corruption, read and write EIO, phantom writes, sync lies, and the
/// damaging crash outcomes): a weight of 4 makes a sector four times as
/// likely to be hit, 0 makes it immune, and a weight never creates a family
/// the configuration leaves at zero. A misdirected write honours only the
/// immunity, and recovery mode silences every family whatever its weight.
///
/// The random draws are made whatever the weights, so installing or
/// changing a focus never shifts the random stream. The
/// [`StorageEligibilityMask`] still vetoes first. A focus belongs to one
/// process's disk (set through
/// [`SimStorageProvider::focus_faults`](crate::SimStorageProvider::focus_faults)):
/// it survives that process's crash and is cleared by a wipe of its storage. Paths are compared after
/// lexical normalization (`./a//b` is `a/b`).
#[derive(Debug, Clone, PartialEq)]
pub struct FaultFocus {
    background: f64,
    /// Per normalized path: `(byte range, weight)`, in insertion order.
    spots: std::collections::BTreeMap<String, Vec<(Range<u64>, f64)>>,
}

impl Default for FaultFocus {
    fn default() -> Self {
        Self::new()
    }
}

impl FaultFocus {
    /// A focus with no spots and a background weight of 1: every sector as
    /// likely as without a focus.
    #[must_use]
    pub fn new() -> Self {
        Self {
            background: 1.0,
            spots: std::collections::BTreeMap::new(),
        }
    }

    /// Weigh every sector no spot covers by `weight` (0 makes them immune).
    /// Negative and non-finite weights count as 0.
    #[must_use]
    pub fn background(mut self, weight: f64) -> Self {
        self.background = sanitize(weight);
        self
    }

    /// Weigh the sectors holding `bytes` of the file at `path` by `weight`.
    /// Where spots overlap, the heaviest wins. Negative and non-finite
    /// weights count as 0.
    #[must_use]
    pub fn spot(mut self, path: &str, bytes: Range<u64>, weight: f64) -> Self {
        if bytes.start < bytes.end {
            self.spots
                .entry(normalize_path(path))
                .or_default()
                .push((bytes, sanitize(weight)));
        }
        self
    }

    /// Weigh every region of a storage format's layout by its kind:
    /// `weight(region)` for each, as [`spot`](Self::spot) would. Any format
    /// that lists its regions as [`LayoutRegion`](moonpool_core::LayoutRegion)s
    /// — `moonpool-journal`'s `JournalAtlas` does — can be aimed this way
    /// without the simulator knowing the format.
    #[must_use]
    pub fn layout<'a>(
        self,
        regions: impl IntoIterator<Item = &'a moonpool_core::LayoutRegion>,
        weight: impl Fn(&moonpool_core::LayoutRegion) -> f64,
    ) -> Self {
        regions.into_iter().fold(self, |focus, region| {
            focus.spot(&region.path, region.bytes.clone(), weight(region))
        })
    }

    /// The weight of `sector` of the file at `path`: the heaviest spot
    /// covering any of its bytes, or the background weight.
    #[must_use]
    pub fn weight(&self, path: &str, sector: u64) -> f64 {
        let size = SECTOR_SIZE as u64;
        let bytes = sector * size..(sector + 1) * size;
        let spots = if is_normal(path) {
            self.spots.get(path)
        } else {
            self.spots.get(&normalize_path(path))
        };
        spots
            .into_iter()
            .flatten()
            .filter(|(range, _)| range.start < bytes.end && bytes.start < range.end)
            .map(|(_, weight)| *weight)
            .reduce(f64::max)
            .unwrap_or(self.background)
    }
}

fn sanitize(weight: f64) -> f64 {
    if weight.is_finite() && weight > 0.0 {
        weight
    } else {
        0.0
    }
}

/// Whether `path` is already in [`normalize_path`]'s form — the engine's
/// resolved paths always are — so a lookup need not allocate.
fn is_normal(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('/')
            .enumerate()
            .all(|(at, part)| !matches!(part, "." | "..") && (!part.is_empty() || at == 0))
        && !path.ends_with('/')
}

/// Lexical normalization: empty and `.` components dropped, `..` folded.
pub(crate) fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    let joined = parts.join("/");
    if path.starts_with('/') {
        format!("/{joined}")
    } else {
        joined
    }
}

/// A probability scaled by a focus weight, still a probability.
pub(crate) fn weighted(probability: f64, weight: f64) -> f64 {
    (probability * weight).min(1.0)
}

/// Storage for an optional [`StorageEligibilityMask`] that can sit inside a
/// `Debug` struct (a closure is not `Debug`).
#[derive(Clone, Default)]
pub(crate) struct EligibilitySlot(Option<StorageEligibilityMask>);

impl EligibilitySlot {
    pub(crate) fn set(&mut self, mask: Option<StorageEligibilityMask>) {
        self.0 = mask;
    }

    /// Whether a random fault may damage this sector.
    pub(crate) fn allows(&self, path: &str, sector: u64) -> bool {
        self.0.as_ref().is_none_or(|mask| mask(path, sector))
    }

    pub(crate) fn mask(&self) -> Option<StorageEligibilityMask> {
        self.0.clone()
    }
}

impl std::fmt::Debug for EligibilitySlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "EligibilitySlot(installed)"
        } else {
            "EligibilitySlot(none)"
        })
    }
}

/// The sector range one byte range touches.
#[must_use]
pub fn sector_range(offset: u64, len: usize) -> Range<u64> {
    let sector = SECTOR_SIZE as u64;
    offset / sector..(offset + len as u64).div_ceil(sector)
}
