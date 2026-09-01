//! Deterministic backing store for simulated block devices.
//!
//! One [`SimBlockStore`] owns every simulated device (keyed by path), a single
//! seeded RNG stream for all fault and crash decisions, the caller-provided
//! eligibility mask, and the observable fault record log.
//!
//! ## Barrier-bounded crash model
//!
//! Each region keeps two images: `committed` (the durable bytes as of the last
//! successful `persist()`) and `visible` (what reads observe). Writes mutate
//! only the visible image and mark their sectors *dirty*; `persist()` copies
//! dirty sectors into the committed image. On [`SimBlockStore::crash_device`],
//! every dirty sector is resolved **independently** to one of
//! [`BlockCrashOutcome`]'s shapes, which is exactly the freedom the
//! [`BlockDevice`](moonpool_core::BlockDevice) contract reserves: writes
//! between two barriers may land in any order, torn at sector granularity.
//!
//! ## Lost-synced-write oracle
//!
//! At every successful `persist()`, each dirty sector is stamped with a CRC of
//! the content the caller was told is durable (FDB `AsyncFileWriteChecker`
//! pattern); the stamp is dropped when the sector is overwritten again. After
//! a crash, a stamped sector whose committed content no longer matches its
//! stamp is a **sim bug** (hard failure) — unless the opt-in barrier-violation
//! family is armed, in which case the mismatch is reported as an expected
//! [`BlockFaultKind::LostSyncedWrite`] event.

use std::{collections::BTreeMap, fmt, ops::Range, sync::Arc};

use moonpool_core::BlockError;
use moonpool_core::block::{RegionId, RegionSpec, SECTOR_SIZE, validate_sector_range};
use parking_lot::Mutex;
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;

use super::BlockFaultConfig;
use crate::storage::SectorBitSet;
use crate::{assert_always, assert_reachable};

/// Eligibility mask consulted before injecting any random fault:
/// `(device path, region, sector) -> eligible`.
///
/// A replication-aware harness can enforce "never fault all copies of one
/// record" without moonpool knowing what a replica is (`TigerBeetle`
/// `ClusterFaultAtlas` pattern). The mask gates the random fault families and
/// the damaging crash outcomes ([`BlockCrashOutcome::Lost`],
/// [`BlockCrashOutcome::LatentFault`], [`BlockCrashOutcome::Shorn`]); an
/// ineligible sector falls back to the always-legal old/new resolution.
/// Random draws are made *before* the mask is consulted, so installing a mask
/// never shifts the RNG stream. The mask is consulted in stable
/// (path, region, ascending sector) order.
pub type BlockEligibilityMask = Arc<dyn Fn(&str, RegionId, u64) -> bool + Send + Sync>;

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

/// Fault family of one recorded fault event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockFaultKind {
    /// A read failed with a simulated I/O error.
    EioRead,
    /// A write failed with a simulated I/O error.
    EioWrite,
    /// A read planted a latent, deterministic sector corruption.
    ReadCorruption,
    /// A write landed at the wrong offset within its region.
    MisdirectedWrite,
    /// A write completed but its bytes never reached the device.
    PhantomWrite,
    /// A `persist()` call failed with a simulated I/O error.
    PersistFailure,
    /// A crash lost a write that `persist()` had reported durable
    /// (barrier-violation family).
    LostSyncedWrite,
    /// A whole device was wiped.
    DeviceWipe,
}

/// One observable fault event injected by the simulated block device.
#[derive(Debug, Clone)]
pub struct BlockFaultRecord {
    /// Path of the affected device.
    pub path: String,
    /// Fault family.
    pub kind: BlockFaultKind,
    /// Affected region, when the fault is region-scoped.
    pub region: Option<RegionId>,
    /// Affected sector range, when the fault is sector-scoped.
    pub sectors: Option<Range<u64>>,
}

/// Resolution shape of one dirty sector during a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockCrashOutcome {
    /// The sector reverted to its last durable contents.
    KeptOld,
    /// The buffered write survived intact.
    KeptNew,
    /// The sector reverted to never-written and reads the device fill pattern
    /// (zeros or garbage, chosen per seed).
    Lost,
    /// The buffered write landed, but reads return deterministically
    /// corrupted bytes (identical on retry).
    LatentFault,
    /// A sub-sector prefix/suffix mix of old and new bytes (opt-in;
    /// weakens the sector-atomicity clause).
    Shorn,
}

/// Crash resolution of one sector, reported by [`SimBlockStore::crash_device`].
#[derive(Debug, Clone)]
pub struct BlockSectorResolution {
    /// Region containing the sector.
    pub region: RegionId,
    /// Sector index within the region.
    pub sector: u64,
    /// How the sector resolved.
    pub outcome: BlockCrashOutcome,
    /// Whether the sector was part of a correlated rollback run.
    pub correlated: bool,
}

/// Full report of one simulated crash.
#[derive(Debug, Clone, Default)]
pub struct BlockCrashReport {
    /// Path of the crashed device.
    pub path: String,
    /// Whether a device existed at the path at all.
    pub existed: bool,
    /// The device vanished entirely: it was created but never persisted, so
    /// the atomic-create contract erases it.
    pub unlinked: bool,
    /// The crash was fully clean: every buffered write survived.
    pub clean: bool,
    /// Per-sector resolutions of the dirty sectors.
    pub resolutions: Vec<BlockSectorResolution>,
    /// Regions whose unpersisted grow was reverted.
    pub grow_reverted: Vec<RegionId>,
    /// Sectors reported durable by `persist()` that the crash lost anyway —
    /// non-empty only with the barrier-violation family armed.
    pub lost_synced: Vec<(RegionId, u64)>,
}

const SECTOR_BYTES: u64 = SECTOR_SIZE as u64;

struct RegionState {
    name: &'static str,
    /// Durable bytes as of the last successful `persist()`.
    committed: Vec<u8>,
    committed_written: SectorBitSet,
    /// Bytes observed by reads (committed + buffered writes).
    visible: Vec<u8>,
    written: SectorBitSet,
    /// Sectors modified since the last successful `persist()`.
    dirty: SectorBitSet,
    /// Sectors `persist()` lied about (barrier-violation family): reported
    /// durable, left volatile.
    lied: SectorBitSet,
    /// Sectors with a latent read fault (deterministic corruption on read).
    faults: SectorBitSet,
    eio_read: SectorBitSet,
    eio_write: SectorBitSet,
    /// Per-sector CRC of the content the caller believes durable.
    oracle: BTreeMap<u64, u32>,
    /// Byte ranges of writes currently in flight (overlap detection).
    in_flight: Vec<Range<u64>>,
}

impl RegionState {
    fn new(name: &'static str, size: u64) -> Self {
        let size_usize = usize::try_from(size).expect("region size fits in usize");
        let sectors = size_usize / SECTOR_SIZE;
        Self {
            name,
            committed: Vec::new(),
            committed_written: SectorBitSet::new(0),
            visible: vec![0; size_usize],
            written: SectorBitSet::new(sectors),
            dirty: SectorBitSet::new(sectors),
            lied: SectorBitSet::new(sectors),
            faults: SectorBitSet::new(sectors),
            eio_read: SectorBitSet::new(sectors),
            eio_write: SectorBitSet::new(sectors),
            oracle: BTreeMap::new(),
            in_flight: Vec::new(),
        }
    }

    fn visible_size(&self) -> u64 {
        self.visible.len() as u64
    }

    fn visible_sectors(&self) -> u64 {
        self.visible_size() / SECTOR_BYTES
    }

    fn committed_sectors(&self) -> u64 {
        (self.committed.len() as u64) / SECTOR_BYTES
    }

    fn sector_slice(bytes: &[u8], sector: u64) -> &[u8] {
        let start = usize::try_from(sector * SECTOR_BYTES).expect("sector offset fits in usize");
        &bytes[start..start + SECTOR_SIZE]
    }

    fn sector_slice_mut(bytes: &mut [u8], sector: u64) -> &mut [u8] {
        let start = usize::try_from(sector * SECTOR_BYTES).expect("sector offset fits in usize");
        &mut bytes[start..start + SECTOR_SIZE]
    }

    /// Grow the *visible* image (bitsets included) to `new_size` bytes,
    /// filling the extension with the device fill pattern.
    fn grow_visible(&mut self, new_size: u64, fill: &FillPattern, region: RegionId) {
        let old_sectors = self.visible_sectors();
        let new_size_usize = usize::try_from(new_size).expect("region size fits in usize");
        self.visible.resize(new_size_usize, 0);
        let new_sectors = new_size / SECTOR_BYTES;
        for sector in old_sectors..new_sectors {
            let slice = Self::sector_slice_mut(&mut self.visible, sector);
            slice.copy_from_slice(&fill.sector(region, sector));
        }
        self.resize_visible_bitsets(new_sectors);
    }

    fn resize_visible_bitsets(&mut self, new_sectors: u64) {
        let n = usize::try_from(new_sectors).expect("sector count fits in usize");
        self.written = SectorBitSet::resize_copy(&self.written, n);
        self.dirty = SectorBitSet::resize_copy(&self.dirty, n);
        self.lied = SectorBitSet::resize_copy(&self.lied, n);
        self.faults = SectorBitSet::resize_copy(&self.faults, n);
        self.eio_read = SectorBitSet::resize_copy(&self.eio_read, n);
        self.eio_write = SectorBitSet::resize_copy(&self.eio_write, n);
    }

    /// Grow the *committed* image to match the visible size, filling the
    /// extension with the device fill pattern.
    fn grow_committed_to_visible(&mut self, fill: &FillPattern, region: RegionId) {
        let old_sectors = self.committed_sectors();
        self.committed.resize(self.visible.len(), 0);
        let new_sectors = self.committed_sectors();
        for sector in old_sectors..new_sectors {
            let slice = Self::sector_slice_mut(&mut self.committed, sector);
            slice.copy_from_slice(&fill.sector(region, sector));
        }
        let n = usize::try_from(new_sectors).expect("sector count fits in usize");
        self.committed_written = SectorBitSet::resize_copy(&self.committed_written, n);
    }

    fn dirty_sectors(&self) -> Vec<u64> {
        (0..self.visible_sectors())
            .filter(|&s| {
                self.dirty
                    .is_set(usize::try_from(s).expect("sector index fits in usize"))
            })
            .collect()
    }
}

/// Deterministic fill pattern for never-written and lost sectors: zeros or
/// per-sector garbage, chosen once per device from the store's seeded RNG.
struct FillPattern {
    garbage: bool,
    seed: u64,
}

impl FillPattern {
    fn sector(&self, region: RegionId, sector: u64) -> [u8; SECTOR_SIZE] {
        let mut bytes = [0u8; SECTOR_SIZE];
        if self.garbage {
            let mix = self
                .seed
                .wrapping_add(u64::from(region.0).wrapping_mul(0x9E37_79B9_7F4A_7C15))
                .wrapping_add(sector);
            let mut rng = ChaCha8Rng::seed_from_u64(mix);
            rng.fill(&mut bytes);
        }
        bytes
    }
}

struct DeviceState {
    /// A device is invisible to `open()` until its first successful
    /// `persist()`; a crash before that erases it entirely (atomic create).
    linked: bool,
    fill: FillPattern,
    regions: Vec<RegionState>,
}

struct StoreState {
    rng: ChaCha8Rng,
    config: BlockFaultConfig,
    /// Whether the barrier-violation family was ever armed on this store.
    ///
    /// Latched at construction rather than read back off `config`, so
    /// [`SimBlockStore::disable_fault_injection`] can stop *new* lies without
    /// retroactively turning a sector a pre-cutoff `persist()` already lied
    /// about into an impossible-state panic in the crash oracle.
    barrier_violation_armed: bool,
    devices: BTreeMap<String, DeviceState>,
    eligibility: Option<BlockEligibilityMask>,
    fault_records: Vec<BlockFaultRecord>,
}

impl StoreState {
    fn record(
        &mut self,
        path: &str,
        kind: BlockFaultKind,
        region: Option<RegionId>,
        sectors: Option<Range<u64>>,
    ) {
        self.fault_records.push(BlockFaultRecord {
            path: path.to_string(),
            kind,
            region,
            sectors,
        });
    }

    fn device(&mut self, path: &str) -> Result<&mut DeviceState, BlockError> {
        self.devices
            .get_mut(path)
            .ok_or_else(|| BlockError::NotFound {
                path: path.to_string(),
            })
    }
}

/// Shared deterministic store behind every [`SimBlockDevice`](super::SimBlockDevice).
///
/// Cloning is cheap and shares state. All randomness (fault firing, crash
/// resolution, fill patterns) is drawn from one `ChaCha8` stream seeded at
/// construction: the same seed and operation sequence produce bit-identical
/// device states, crash resolutions, and fault firings. Inside a simulation,
/// derive the seed from the iteration seed (e.g. `current_sim_seed()`).
#[derive(Clone)]
pub struct SimBlockStore {
    inner: Arc<Mutex<StoreState>>,
}

impl fmt::Debug for SimBlockStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.inner.lock();
        f.debug_struct("SimBlockStore")
            .field("devices", &inner.devices.keys().collect::<Vec<_>>())
            .field("fault_records", &inner.fault_records.len())
            .finish()
    }
}

impl SimBlockStore {
    /// Create a store with the given RNG seed and fault configuration.
    #[must_use]
    pub fn new(seed: u64, config: BlockFaultConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(StoreState {
                rng: ChaCha8Rng::seed_from_u64(seed),
                barrier_violation_armed: config.barrier_violation_probability > 0.0,
                config,
                devices: BTreeMap::new(),
                eligibility: None,
                fault_records: Vec::new(),
            })),
        }
    }

    /// Install the fault eligibility mask (see [`BlockEligibilityMask`]).
    pub fn set_eligibility_mask(&self, mask: BlockEligibilityMask) {
        self.inner.lock().eligibility = Some(mask);
    }

    /// Remove the fault eligibility mask: every sector becomes eligible.
    pub fn clear_eligibility_mask(&self) {
        self.inner.lock().eligibility = None;
    }

    /// Stop injecting new device faults (see
    /// [`BlockFaultConfig::disable_fault_injection`]).
    ///
    /// Damage already on the device is untouched, and the crash model keeps
    /// the shape it was built with.
    pub fn disable_fault_injection(&self) {
        self.inner.lock().config.disable_fault_injection();
    }

    /// Drain the fault records accumulated so far.
    #[must_use]
    pub fn take_fault_records(&self) -> Vec<BlockFaultRecord> {
        std::mem::take(&mut self.inner.lock().fault_records)
    }

    /// Whether a device at `path` is visible to `open()` (created and
    /// persisted at least once).
    #[must_use]
    pub fn device_exists(&self, path: &str) -> bool {
        self.inner
            .lock()
            .devices
            .get(path)
            .is_some_and(|device| device.linked)
    }

    // ------------------------------------------------------------------
    // Targeted fault API (directed red tests)
    // ------------------------------------------------------------------

    /// Plant a latent read fault on each sector in `sectors`: reads return
    /// deterministically corrupted bytes (identical on retry) until the
    /// sectors are rewritten.
    ///
    /// # Errors
    ///
    /// Returns [`BlockError::NotFound`] / [`BlockError::InvalidArgument`] for
    /// an unknown device, region, or out-of-bounds sector range.
    pub fn corrupt(
        &self,
        path: &str,
        region: RegionId,
        sectors: Range<u64>,
    ) -> Result<(), BlockError> {
        self.with_sector_bits(path, region, sectors, |state, sector| {
            state.faults.set(sector);
        })
    }

    /// Make every read and/or write touching `sectors` fail with
    /// [`BlockError::Io`] until [`SimBlockStore::clear_eio`] is called.
    ///
    /// # Errors
    ///
    /// Returns [`BlockError::NotFound`] / [`BlockError::InvalidArgument`] for
    /// an unknown device, region, or out-of-bounds sector range.
    pub fn fail_with_eio(
        &self,
        path: &str,
        region: RegionId,
        sectors: Range<u64>,
        target: EioTarget,
    ) -> Result<(), BlockError> {
        self.with_sector_bits(path, region, sectors, |state, sector| {
            if matches!(target, EioTarget::Read | EioTarget::ReadWrite) {
                state.eio_read.set(sector);
            }
            if matches!(target, EioTarget::Write | EioTarget::ReadWrite) {
                state.eio_write.set(sector);
            }
        })
    }

    /// Clear targeted EIO injections on a region.
    ///
    /// # Errors
    ///
    /// Returns [`BlockError::NotFound`] / [`BlockError::InvalidArgument`] for
    /// an unknown device or region.
    pub fn clear_eio(
        &self,
        path: &str,
        region: RegionId,
        target: EioTarget,
    ) -> Result<(), BlockError> {
        let mut inner = self.inner.lock();
        let device = inner.device(path)?;
        let state = region_state_mut(device, region)?;
        for sector in 0..state.eio_read.len() {
            if matches!(target, EioTarget::Read | EioTarget::ReadWrite) {
                state.eio_read.clear(sector);
            }
            if matches!(target, EioTarget::Write | EioTarget::ReadWrite) {
                state.eio_write.clear(sector);
            }
        }
        Ok(())
    }

    /// Remove a device entirely, as if its media were destroyed.
    ///
    /// # Errors
    ///
    /// Returns [`BlockError::NotFound`] if no device exists at `path`.
    pub fn wipe_device(&self, path: &str) -> Result<(), BlockError> {
        let mut inner = self.inner.lock();
        if inner.devices.remove(path).is_none() {
            return Err(BlockError::NotFound {
                path: path.to_string(),
            });
        }
        inner.record(path, BlockFaultKind::DeviceWipe, None, None);
        Ok(())
    }

    /// Remove every device in the store, as if the whole disk were replaced.
    /// Returns how many devices were wiped.
    #[must_use]
    pub fn wipe_all(&self) -> usize {
        let mut inner = self.inner.lock();
        let paths: Vec<String> = inner.devices.keys().cloned().collect();
        inner.devices.clear();
        for path in &paths {
            inner.record(path, BlockFaultKind::DeviceWipe, None, None);
        }
        paths.len()
    }

    /// Deliberately mutate a *committed* sector out-of-band, bypassing the
    /// crash model. This simulates a sim bug for oracle tests: the next crash
    /// of the device must fail loudly (unless the barrier-violation family is
    /// armed, which this helper does not set).
    ///
    /// # Errors
    ///
    /// Returns [`BlockError::NotFound`] / [`BlockError::InvalidArgument`] for
    /// an unknown device, region, or out-of-bounds sector.
    pub fn corrupt_committed_out_of_band(
        &self,
        path: &str,
        region: RegionId,
        sector: u64,
    ) -> Result<(), BlockError> {
        let mut inner = self.inner.lock();
        let device = inner.device(path)?;
        let state = region_state_mut(device, region)?;
        if sector >= state.committed_sectors() {
            return Err(BlockError::invalid_argument(format!(
                "sector {sector} is beyond the committed image"
            )));
        }
        let slice = RegionState::sector_slice_mut(&mut state.committed, sector);
        slice[0] ^= 0xFF;
        Ok(())
    }

    fn with_sector_bits(
        &self,
        path: &str,
        region: RegionId,
        sectors: Range<u64>,
        mut apply: impl FnMut(&mut RegionState, usize),
    ) -> Result<(), BlockError> {
        let mut inner = self.inner.lock();
        let device = inner.device(path)?;
        let state = region_state_mut(device, region)?;
        if sectors.end > state.visible_sectors() {
            return Err(BlockError::invalid_argument(format!(
                "sector range {sectors:?} exceeds region size"
            )));
        }
        for sector in sectors {
            apply(
                state,
                usize::try_from(sector).expect("sector index fits in usize"),
            );
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Device lifecycle
    // ------------------------------------------------------------------

    pub(crate) fn create_device(
        &self,
        path: &str,
        regions: &[RegionSpec],
    ) -> Result<(), BlockError> {
        let mut inner = self.inner.lock();
        if inner.devices.contains_key(path) {
            return Err(BlockError::AlreadyExists {
                path: path.to_string(),
            });
        }
        let mut states = Vec::with_capacity(regions.len());
        for spec in regions {
            if spec.size % SECTOR_BYTES != 0 {
                return Err(BlockError::invalid_argument(format!(
                    "region '{}' size {} is not a sector multiple",
                    spec.name, spec.size
                )));
            }
            states.push(RegionState::new(spec.name, spec.size));
        }
        let garbage = inner.rng.random::<f64>() < inner.config.garbage_fill_probability;
        let fill_seed = inner.rng.random::<u64>();
        if garbage {
            assert_reachable!("block: device uses garbage fill for unwritten sectors");
        } else {
            assert_reachable!("block: device uses zero fill for unwritten sectors");
        }
        let fill = FillPattern {
            garbage,
            seed: fill_seed,
        };
        let mut device = DeviceState {
            linked: false,
            fill,
            regions: states,
        };
        // Materialize the fill pattern into the freshly allocated regions.
        for (index, state) in device.regions.iter_mut().enumerate() {
            let region = RegionId(u32::try_from(index).expect("region count fits in u32"));
            for sector in 0..state.visible_sectors() {
                let bytes = device.fill.sector(region, sector);
                RegionState::sector_slice_mut(&mut state.visible, sector).copy_from_slice(&bytes);
            }
        }
        inner.devices.insert(path.to_string(), device);
        Ok(())
    }

    pub(crate) fn open_device(&self, path: &str) -> Result<(), BlockError> {
        let inner = self.inner.lock();
        match inner.devices.get(path) {
            Some(device) if device.linked => Ok(()),
            _ => Err(BlockError::NotFound {
                path: path.to_string(),
            }),
        }
    }

    /// # Panics
    ///
    /// Panics if the device or region does not exist (contract: `region_size`
    /// on an invalid region is a caller bug).
    pub(crate) fn region_size(&self, path: &str, region: RegionId) -> u64 {
        let inner = self.inner.lock();
        let device = inner
            .devices
            .get(path)
            .unwrap_or_else(|| panic!("region_size on unknown block device {path}"));
        let index = usize::try_from(region.0).expect("region index fits in usize");
        device
            .regions
            .get(index)
            .unwrap_or_else(|| panic!("region_size on unknown region {region:?} of {path}"))
            .visible_size()
    }

    pub(crate) fn region_count(&self, path: &str) -> u32 {
        let inner = self.inner.lock();
        inner.devices.get(path).map_or(0, |device| {
            u32::try_from(device.regions.len()).expect("region count fits in u32")
        })
    }

    // ------------------------------------------------------------------
    // I/O operations (called from SimBlockDevice around a yield point)
    // ------------------------------------------------------------------

    pub(crate) fn validate_read(
        &self,
        path: &str,
        region: RegionId,
        offset: u64,
        len: usize,
    ) -> Result<(), BlockError> {
        let mut inner = self.inner.lock();
        let device = inner.device(path)?;
        let state = region_state_mut(device, region)?;
        validate_sector_range(offset, len, state.visible_size())
    }

    pub(crate) fn finish_read(
        &self,
        path: &str,
        region: RegionId,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<(), BlockError> {
        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        let device = inner
            .devices
            .get_mut(path)
            .ok_or_else(|| BlockError::NotFound {
                path: path.to_string(),
            })?;
        let state = region_state_mut(device, region)?;
        validate_sector_range(offset, buf.len(), state.visible_size())?;
        let sectors = sector_range(offset, buf.len());

        // EIO: targeted injections fire unconditionally; the random family is
        // rolled first, then gated by config and mask.
        let eio_roll = inner.rng.random::<f64>();
        let targeted = range_has_bit(&state.eio_read, sectors.clone());
        let random_hit =
            inner.config.eio_read_probability > 0.0 && eio_roll < inner.config.eio_read_probability;
        if targeted
            || (random_hit
                && mask_allows(inner.eligibility.as_ref(), path, region, sectors.clone()))
        {
            assert_reachable!("block fault: read failed with EIO");
            record_fault(
                &mut inner.fault_records,
                path,
                BlockFaultKind::EioRead,
                region,
                sectors,
            );
            return Err(BlockError::io("simulated read I/O error"));
        }

        // Read-time corruption plants latent faults per sector.
        let mut corrupted = false;
        for sector in sectors.clone() {
            let roll = inner.rng.random::<f64>();
            if inner.config.read_corruption_probability > 0.0
                && roll < inner.config.read_corruption_probability
                && eligible_one(inner.eligibility.as_ref(), path, region, sector)
            {
                state
                    .faults
                    .set(usize::try_from(sector).expect("sector index fits in usize"));
                corrupted = true;
            }
        }
        if corrupted {
            assert_reachable!("block fault: read planted latent corruption");
            record_fault(
                &mut inner.fault_records,
                path,
                BlockFaultKind::ReadCorruption,
                region,
                sectors.clone(),
            );
        }

        let offset_usize = usize::try_from(offset).expect("offset fits in usize");
        buf.copy_from_slice(&state.visible[offset_usize..offset_usize + buf.len()]);
        for sector in sectors {
            if state
                .faults
                .is_set(usize::try_from(sector).expect("sector index fits in usize"))
            {
                let pristine = RegionState::sector_slice(&state.visible, sector);
                let start = usize::try_from(sector * SECTOR_BYTES).expect("fits in usize");
                corrupt_sector(
                    pristine,
                    &mut buf[start - offset_usize..start - offset_usize + SECTOR_SIZE],
                );
            }
        }
        Ok(())
    }

    pub(crate) fn begin_write(
        &self,
        path: &str,
        region: RegionId,
        offset: u64,
        len: usize,
    ) -> Result<(), BlockError> {
        let mut inner = self.inner.lock();
        let device = inner.device(path)?;
        let state = region_state_mut(device, region)?;
        validate_sector_range(offset, len, state.visible_size())?;
        let range = offset..offset + len as u64;
        let overlaps = state
            .in_flight
            .iter()
            .any(|r| r.start < range.end && range.start < r.end);
        assert_always!(
            !overlaps,
            "block: no concurrent overlapping writes to one region",
            { "path" => path, "region" => region.0, "offset" => offset }
        );
        state.in_flight.push(range);
        Ok(())
    }

    /// Drop the in-flight write reservation taken by
    /// [`begin_write`](Self::begin_write). Called from the device layer's
    /// RAII guard so a write future dropped mid-flight (e.g. inside a
    /// timeout) cannot leak its reservation and poison the overlap
    /// assertion.
    pub(crate) fn release_write_reservation(
        &self,
        path: &str,
        region: RegionId,
        offset: u64,
        len: usize,
    ) {
        let mut inner = self.inner.lock();
        let Ok(device) = inner.device(path) else {
            return;
        };
        let Ok(state) = region_state_mut(device, region) else {
            return;
        };
        let range = offset..offset + len as u64;
        if let Some(position) = state.in_flight.iter().position(|r| *r == range) {
            state.in_flight.remove(position);
        }
    }

    pub(crate) fn finish_write(
        &self,
        path: &str,
        region: RegionId,
        offset: u64,
        data: &[u8],
    ) -> Result<(), BlockError> {
        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        let device = inner
            .devices
            .get_mut(path)
            .ok_or_else(|| BlockError::NotFound {
                path: path.to_string(),
            })?;
        let state = region_state_mut(device, region)?;
        validate_sector_range(offset, data.len(), state.visible_size())?;
        let sectors = sector_range(offset, data.len());
        let mask = inner.eligibility.as_ref();

        // EIO.
        let eio_roll = inner.rng.random::<f64>();
        let targeted = range_has_bit(&state.eio_write, sectors.clone());
        let random_hit = inner.config.eio_write_probability > 0.0
            && eio_roll < inner.config.eio_write_probability;
        if targeted || (random_hit && mask_allows(mask, path, region, sectors.clone())) {
            assert_reachable!("block fault: write failed with EIO");
            record_fault(
                &mut inner.fault_records,
                path,
                BlockFaultKind::EioWrite,
                region,
                sectors,
            );
            return Err(BlockError::io("simulated write I/O error"));
        }

        // Phantom write: acknowledged, never applied.
        let phantom_roll = inner.rng.random::<f64>();
        if inner.config.phantom_write_probability > 0.0
            && phantom_roll < inner.config.phantom_write_probability
            && mask_allows(mask, path, region, sectors.clone())
        {
            assert_reachable!("block fault: phantom write dropped");
            record_fault(
                &mut inner.fault_records,
                path,
                BlockFaultKind::PhantomWrite,
                region,
                sectors,
            );
            return Ok(());
        }

        // Misdirected write: lands at the wrong sector-aligned offset within
        // the same region.
        let misdirect_roll = inner.rng.random::<f64>();
        let write_sectors = sectors.end - sectors.start;
        let region_sectors = state.visible_sectors();
        if inner.config.misdirected_write_probability > 0.0
            && misdirect_roll < inner.config.misdirected_write_probability
            && region_sectors > write_sectors
        {
            let max_start = region_sectors - write_sectors;
            let mut mistaken = inner.rng.random_range(0..=max_start);
            if mistaken == sectors.start {
                mistaken = (mistaken + 1) % (max_start + 1);
            }
            let mistaken_range = mistaken..mistaken + write_sectors;
            if mistaken != sectors.start
                && mask_allows(mask, path, region, sectors.clone())
                && mask_allows(mask, path, region, mistaken_range.clone())
            {
                assert_reachable!("block fault: misdirected write within region");
                record_fault(
                    &mut inner.fault_records,
                    path,
                    BlockFaultKind::MisdirectedWrite,
                    region,
                    mistaken_range,
                );
                apply_write(state, mistaken * SECTOR_BYTES, data);
                return Ok(());
            }
        }

        apply_write(state, offset, data);
        Ok(())
    }

    pub(crate) fn do_grow(
        &self,
        path: &str,
        region: RegionId,
        new_size: u64,
    ) -> Result<(), BlockError> {
        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        let device = inner
            .devices
            .get_mut(path)
            .ok_or_else(|| BlockError::NotFound {
                path: path.to_string(),
            })?;
        let fill_garbage = device.fill.garbage;
        let fill_seed = device.fill.seed;
        let state = region_state_mut(device, region)?;
        if !new_size.is_multiple_of(SECTOR_BYTES) {
            return Err(BlockError::invalid_argument(format!(
                "grow size {new_size} is not a sector multiple"
            )));
        }
        if new_size < state.visible_size() {
            return Err(BlockError::invalid_argument(format!(
                "grow is grow-only: {new_size} < current size {}",
                state.visible_size()
            )));
        }
        let fill = FillPattern {
            garbage: fill_garbage,
            seed: fill_seed,
        };
        state.grow_visible(new_size, &fill, region);
        Ok(())
    }

    pub(crate) fn do_persist(&self, path: &str) -> Result<(), BlockError> {
        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        let device = inner
            .devices
            .get_mut(path)
            .ok_or_else(|| BlockError::NotFound {
                path: path.to_string(),
            })?;

        let failure_roll = inner.rng.random::<f64>();
        if inner.config.persist_failure_probability > 0.0
            && failure_roll < inner.config.persist_failure_probability
        {
            assert_reachable!("block fault: persist failed");
            inner.fault_records.push(BlockFaultRecord {
                path: path.to_string(),
                kind: BlockFaultKind::PersistFailure,
                region: None,
                sectors: None,
            });
            return Err(BlockError::io("simulated persist I/O error"));
        }

        let lie_probability = inner.config.barrier_violation_probability;
        for (index, state) in device.regions.iter_mut().enumerate() {
            let region = RegionId(u32::try_from(index).expect("region count fits in u32"));
            state.grow_committed_to_visible(&device.fill, region);
            for sector in state.dirty_sectors() {
                let sector_usize = usize::try_from(sector).expect("sector index fits in usize");
                let lied = lie_probability > 0.0
                    && inner.rng.random::<f64>() < lie_probability
                    && inner
                        .eligibility
                        .as_ref()
                        .is_none_or(|mask| mask(path, region, sector));
                let believed = RegionState::sector_slice(&state.visible, sector);
                if state.written.is_set(sector_usize) {
                    state.oracle.insert(sector, crc32c::crc32c(believed));
                }
                if lied {
                    assert_reachable!("block fault: persist lied, synced write left volatile");
                    state.lied.set(sector_usize);
                } else {
                    let bytes = believed.to_vec();
                    RegionState::sector_slice_mut(&mut state.committed, sector)
                        .copy_from_slice(&bytes);
                    if state.written.is_set(sector_usize) {
                        state.committed_written.set(sector_usize);
                    } else {
                        state.committed_written.clear(sector_usize);
                    }
                    state.dirty.clear(sector_usize);
                    state.lied.clear(sector_usize);
                }
            }
        }
        device.linked = true;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Barrier-bounded crash model
// ---------------------------------------------------------------------------

impl SimBlockStore {
    /// Simulate a crash of the device at `path`.
    ///
    /// Every sector written since the last successful `persist()` is resolved
    /// independently through the barrier-bounded crash model (see the module
    /// docs): kept old, kept new, lost to the fill pattern, left with a
    /// latent read fault, or (opt-in) shorn — with an occasional fully-clean
    /// crash and occasional correlated rollback of a contiguous sector run.
    /// A device created but never persisted vanishes entirely (atomic
    /// create). Afterwards the lost-synced-write oracle sweeps every stamped
    /// sector.
    ///
    /// # Panics
    ///
    /// Panics when the oracle finds a sector that `persist()` reported
    /// durable changed across the crash while the barrier-violation family is
    /// not armed — that is a simulator bug, never legal device behavior.
    #[must_use]
    pub fn crash_device(&self, path: &str) -> BlockCrashReport {
        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        let mut report = BlockCrashReport {
            path: path.to_string(),
            ..BlockCrashReport::default()
        };
        let Some(device) = inner.devices.get_mut(path) else {
            return report;
        };
        report.existed = true;
        if !device.linked {
            assert_reachable!("block crash: unpersisted device erased (atomic create)");
            inner.devices.remove(path);
            report.unlinked = true;
            return report;
        }

        let clean = inner.rng.random::<f64>() < inner.config.clean_crash_probability;
        let mut lied_per_region: Vec<Vec<u64>> = Vec::with_capacity(device.regions.len());
        if clean {
            assert_reachable!("block crash: clean crash preserved all buffered writes");
            report.clean = true;
            for (index, state) in device.regions.iter_mut().enumerate() {
                let region = RegionId(u32::try_from(index).expect("region count fits in u32"));
                state.grow_committed_to_visible(&device.fill, region);
                for sector in state.dirty_sectors() {
                    commit_sector(state, sector);
                }
                lied_per_region.push(Vec::new());
            }
        } else {
            let mut ctx = CrashCtx {
                rng: &mut inner.rng,
                config: &inner.config,
                mask: inner.eligibility.as_ref(),
                path,
            };
            for (index, state) in device.regions.iter_mut().enumerate() {
                let region = RegionId(u32::try_from(index).expect("region count fits in u32"));
                let lied = resolve_region_crash(&mut ctx, region, state, &device.fill, &mut report);
                lied_per_region.push(lied);
            }
        }

        let armed = inner.barrier_violation_armed;
        for (index, state) in device.regions.iter_mut().enumerate() {
            let region = RegionId(u32::try_from(index).expect("region count fits in u32"));
            oracle_sweep(
                state,
                path,
                region,
                &lied_per_region[index],
                armed,
                &mut inner.fault_records,
                &mut report,
            );
        }
        report
    }

    /// Simulate a crash of every device in the store (stable path order).
    #[must_use]
    pub fn crash_all(&self) -> Vec<BlockCrashReport> {
        let paths: Vec<String> = self.inner.lock().devices.keys().cloned().collect();
        paths.iter().map(|path| self.crash_device(path)).collect()
    }
}

/// Commit one dirty sector honestly: visible bytes become durable.
fn commit_sector(state: &mut RegionState, sector: u64) {
    let sector_usize = usize::try_from(sector).expect("sector index fits in usize");
    let start = usize::try_from(sector * SECTOR_BYTES).expect("sector offset fits in usize");
    let (committed, visible) = (&mut state.committed, &state.visible);
    committed[start..start + SECTOR_SIZE].copy_from_slice(&visible[start..start + SECTOR_SIZE]);
    if state.written.is_set(sector_usize) {
        state.committed_written.set(sector_usize);
    } else {
        state.committed_written.clear(sector_usize);
    }
    state.dirty.clear(sector_usize);
    state.lied.clear(sector_usize);
}

/// Split borrows of the store shared by the crash-resolution helpers.
struct CrashCtx<'a> {
    rng: &'a mut ChaCha8Rng,
    config: &'a BlockFaultConfig,
    mask: Option<&'a BlockEligibilityMask>,
    path: &'a str,
}

/// Resolve one region's dirty sectors through the crash model; returns the
/// sectors `persist()` had lied about (for the oracle sweep).
fn resolve_region_crash(
    ctx: &mut CrashCtx<'_>,
    region: RegionId,
    state: &mut RegionState,
    fill: &FillPattern,
    report: &mut BlockCrashReport,
) -> Vec<u64> {
    // Unpersisted grow resolves first: the new size either survives or the
    // region reverts to its last durable size (dropping buffered writes in
    // the reverted tail).
    if state.visible.len() > state.committed.len() {
        if ctx.rng.random::<f64>() < ctx.config.grow_survives_crash_probability {
            assert_reachable!("block crash: unpersisted grow survived");
            state.grow_committed_to_visible(fill, region);
        } else {
            assert_reachable!("block crash: unpersisted grow reverted");
            let committed_len = state.committed.len();
            state.visible.truncate(committed_len);
            let sectors = state.committed_sectors();
            state.resize_visible_bitsets(sectors);
            report.grow_reverted.push(region);
        }
    }

    let dirty = state.dirty_sectors();
    let lied_sectors: Vec<u64> = dirty
        .iter()
        .copied()
        .filter(|&s| {
            state
                .lied
                .is_set(usize::try_from(s).expect("sector index fits in usize"))
        })
        .collect();

    // Correlated rollback: a contiguous run of sectors reverts together
    // (erase-block damage, Zheng FAST'13).
    let mut window: Option<Range<u64>> = None;
    if !dirty.is_empty()
        && ctx.config.correlated_rollback_probability > 0.0
        && ctx.rng.random::<f64>() < ctx.config.correlated_rollback_probability
    {
        let anchor = dirty[ctx.rng.random_range(0..dirty.len())];
        let run = ctx
            .rng
            .random_range(1..=ctx.config.correlated_rollback_max_run.max(1));
        window = Some(anchor..anchor.saturating_add(run));
    }
    let mut correlated_fired = false;

    for sector in dirty {
        let sector_usize = usize::try_from(sector).expect("sector index fits in usize");
        let in_window = window.as_ref().is_some_and(|w| w.contains(&sector));
        correlated_fired |= in_window;
        let lied = state.lied.is_set(sector_usize);
        let outcome = choose_outcome(ctx, region, sector, lied, in_window);
        apply_outcome(ctx.rng, state, fill, region, sector, outcome);
        note_outcome_reachable(outcome, fill.garbage);
        report.resolutions.push(BlockSectorResolution {
            region,
            sector,
            outcome,
            correlated: in_window,
        });
    }
    if correlated_fired {
        assert_reachable!("block crash: correlated rollback of a sector run");
    }

    // Converge: the surviving durable image is what the rebooted process sees.
    state.visible.copy_from_slice(&state.committed);
    let sectors = usize::try_from(state.committed_sectors()).expect("sector count fits in usize");
    state.written = SectorBitSet::resize_copy(&state.committed_written, sectors);
    state.dirty = SectorBitSet::new(sectors);
    state.lied = SectorBitSet::new(sectors);
    lied_sectors
}

/// Pick the resolution shape for one dirty sector.
///
/// Damaging shapes (lost / latent fault / shorn) are gated by the eligibility
/// mask: an ineligible sector falls back to a plain rollback. Sectors
/// `persist()` lied about always roll back — the whole point of the lie is
/// that the write was never durable.
fn choose_outcome(
    ctx: &mut CrashCtx<'_>,
    region: RegionId,
    sector: u64,
    lied: bool,
    in_window: bool,
) -> BlockCrashOutcome {
    if in_window || lied {
        return BlockCrashOutcome::KeptOld;
    }
    let roll = ctx.rng.random::<f64>();
    let eligible = eligible_one(ctx.mask, ctx.path, region, sector);
    let lost_at = ctx.config.crash_lost_probability;
    let latent_at = lost_at + ctx.config.crash_latent_fault_probability;
    let shorn_at = latent_at + ctx.config.shorn_write_probability;
    if roll < lost_at {
        return if eligible {
            BlockCrashOutcome::Lost
        } else {
            BlockCrashOutcome::KeptOld
        };
    }
    if roll < latent_at {
        return if eligible {
            BlockCrashOutcome::LatentFault
        } else {
            BlockCrashOutcome::KeptOld
        };
    }
    if roll < shorn_at {
        return if eligible {
            BlockCrashOutcome::Shorn
        } else {
            BlockCrashOutcome::KeptOld
        };
    }
    let remaining = (1.0 - shorn_at).max(f64::EPSILON);
    if (roll - shorn_at) / remaining < 0.5 {
        BlockCrashOutcome::KeptOld
    } else {
        BlockCrashOutcome::KeptNew
    }
}

/// Materialize one sector's crash resolution into the committed image.
fn apply_outcome(
    rng: &mut ChaCha8Rng,
    state: &mut RegionState,
    fill: &FillPattern,
    region: RegionId,
    sector: u64,
    outcome: BlockCrashOutcome,
) {
    let sector_usize = usize::try_from(sector).expect("sector index fits in usize");
    let start = usize::try_from(sector * SECTOR_BYTES).expect("sector offset fits in usize");
    match outcome {
        BlockCrashOutcome::KeptOld => {}
        BlockCrashOutcome::KeptNew | BlockCrashOutcome::LatentFault => {
            commit_sector(state, sector);
            if outcome == BlockCrashOutcome::LatentFault {
                state.faults.set(sector_usize);
            }
        }
        BlockCrashOutcome::Lost => {
            let bytes = fill.sector(region, sector);
            state.committed[start..start + SECTOR_SIZE].copy_from_slice(&bytes);
            state.committed_written.clear(sector_usize);
        }
        BlockCrashOutcome::Shorn => {
            let split = rng.random_range(1..SECTOR_SIZE);
            let prefix_new = rng.random::<bool>();
            let (committed, visible) = (&mut state.committed, &state.visible);
            if prefix_new {
                committed[start..start + split].copy_from_slice(&visible[start..start + split]);
            } else {
                committed[start + split..start + SECTOR_SIZE]
                    .copy_from_slice(&visible[start + split..start + SECTOR_SIZE]);
            }
            state.committed_written.set(sector_usize);
        }
    }
}

fn note_outcome_reachable(outcome: BlockCrashOutcome, garbage_fill: bool) {
    match outcome {
        BlockCrashOutcome::KeptOld => {
            assert_reachable!("block crash: sector rolled back to old contents");
        }
        BlockCrashOutcome::KeptNew => {
            assert_reachable!("block crash: sector kept new contents");
        }
        BlockCrashOutcome::Lost => {
            if garbage_fill {
                assert_reachable!("block crash: sector lost to garbage fill");
            } else {
                assert_reachable!("block crash: sector lost to zeros");
            }
        }
        BlockCrashOutcome::LatentFault => {
            assert_reachable!("block crash: sector left with latent read fault");
        }
        BlockCrashOutcome::Shorn => {
            assert_reachable!("block crash: sector shorn sub-sector");
        }
    }
}

/// Verify the lost-synced-write oracle after a crash: every stamped sector's
/// committed content must still match the CRC recorded when `persist()`
/// reported it durable.
fn oracle_sweep(
    state: &mut RegionState,
    path: &str,
    region: RegionId,
    lied: &[u64],
    armed: bool,
    records: &mut Vec<BlockFaultRecord>,
    report: &mut BlockCrashReport,
) {
    let mut lost = Vec::new();
    for (&sector, &crc) in &state.oracle {
        let actual = crc32c::crc32c(RegionState::sector_slice(&state.committed, sector));
        if actual == crc {
            continue;
        }
        assert!(
            armed && lied.contains(&sector),
            "moonpool sim bug: sector {sector} of region '{}' ({region:?}) on device '{path}' \
             was reported durable by persist() but changed across a crash without the \
             barrier-violation family armed",
            state.name,
        );
        lost.push(sector);
    }
    for sector in lost {
        assert_reachable!("block crash: lost a synced write (barrier violation)");
        state.oracle.remove(&sector);
        records.push(BlockFaultRecord {
            path: path.to_string(),
            kind: BlockFaultKind::LostSyncedWrite,
            region: Some(region),
            sectors: Some(sector..sector + 1),
        });
        report.lost_synced.push((region, sector));
    }
}

fn region_state_mut(
    device: &mut DeviceState,
    region: RegionId,
) -> Result<&mut RegionState, BlockError> {
    let index = usize::try_from(region.0).expect("region index fits in usize");
    device
        .regions
        .get_mut(index)
        .ok_or_else(|| BlockError::invalid_argument(format!("unknown region {region:?}")))
}

fn sector_range(offset: u64, len: usize) -> Range<u64> {
    offset / SECTOR_BYTES..(offset + len as u64) / SECTOR_BYTES
}

fn range_has_bit(bits: &SectorBitSet, sectors: Range<u64>) -> bool {
    sectors
        .map(|s| usize::try_from(s).expect("sector index fits in usize"))
        .any(|s| s < bits.len() && bits.is_set(s))
}

fn mask_allows(
    mask: Option<&BlockEligibilityMask>,
    path: &str,
    region: RegionId,
    sectors: Range<u64>,
) -> bool {
    let mut all = true;
    for sector in sectors {
        all &= eligible_one(mask, path, region, sector);
    }
    all
}

fn eligible_one(
    mask: Option<&BlockEligibilityMask>,
    path: &str,
    region: RegionId,
    sector: u64,
) -> bool {
    mask.is_none_or(|mask| mask(path, region, sector))
}

fn record_fault(
    records: &mut Vec<BlockFaultRecord>,
    path: &str,
    kind: BlockFaultKind,
    region: RegionId,
    sectors: Range<u64>,
) {
    records.push(BlockFaultRecord {
        path: path.to_string(),
        kind,
        region: Some(region),
        sectors: Some(sectors),
    });
}

/// Apply a write to the visible image: bytes land, sectors become written and
/// dirty, latent faults are healed, and oracle stamps are dropped (the
/// previous durable content's guarantee is destroyed by the overwrite).
fn apply_write(state: &mut RegionState, offset: u64, data: &[u8]) {
    let offset_usize = usize::try_from(offset).expect("offset fits in usize");
    state.visible[offset_usize..offset_usize + data.len()].copy_from_slice(data);
    for sector in sector_range(offset, data.len()) {
        let sector_usize = usize::try_from(sector).expect("sector index fits in usize");
        state.written.set(sector_usize);
        state.dirty.set(sector_usize);
        state.faults.clear(sector_usize);
        state.lied.clear(sector_usize);
        state.oracle.remove(&sector);
    }
}

/// Deterministically corrupt one sector's read buffer, seeded from the
/// pristine content so retries observe the identical corruption.
fn corrupt_sector(pristine: &[u8], buf: &mut [u8]) {
    let mut seed_bytes = [0u8; 8];
    let n = pristine.len().min(8);
    seed_bytes[..n].copy_from_slice(&pristine[..n]);
    let mut rng = ChaCha8Rng::seed_from_u64(u64::from_le_bytes(seed_bytes));
    let byte_idx = rng.random_range(0..buf.len());
    let bit_idx = rng.random_range(0..8u8);
    buf[byte_idx] ^= 1 << bit_idx;
}
