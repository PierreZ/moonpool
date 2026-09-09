//! The one authoritative image of one simulated file.
//!
//! Every access path — stream reads and writes, positioned reads and writes,
//! syncs, truncation, crash resolution, targeted fault injection — reaches
//! these bytes. There is deliberately no second byte store for a second API.
//!
//! ## Two images, one barrier
//!
//! A file keeps a `committed` image (the bytes as of the last successful sync)
//! and a `visible` image (what reads observe). A write mutates the visible
//! image and marks its sectors *dirty*; a sync copies dirty sectors into the
//! committed image. On a crash every dirty sector resolves **independently**
//! to one of [`CrashOutcome`]'s shapes, which is exactly the freedom a real
//! disk reserves: writes between two syncs may land in any order, torn at
//! sector granularity, and each sector may be old, new, lost, or damaged.
//!
//! ## Lost-synced-write oracle
//!
//! At every successful sync each dirty sector is stamped with a CRC of the
//! content the caller was told is durable (`FoundationDB`'s
//! `AsyncFileWriteChecker` pattern); the stamp is dropped when the sector is
//! overwritten. After a crash, a stamped sector whose committed content no
//! longer matches its stamp is a **simulator bug** (hard failure) — unless
//! the opt-in barrier-violation family is armed, in which case it is the
//! expected [`StorageFaultKind::LostSyncedWrite`] it was armed to produce.
//!
//! ## Determinism
//!
//! Faults are applied on read, never stored in the pristine bytes, and every
//! corruption is seeded from the content it damages: re-reading a damaged
//! sector returns the same damaged bytes, so a retry never heals. All
//! randomness comes from the simulation's one stream.

use std::{collections::BTreeMap, ops::Range};

use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;

use super::{
    StorageConfiguration,
    faults::{CrashOutcome, EioTarget, FileCrashReport, SECTOR_SIZE, SectorResolution},
};
use crate::assert_reachable;
use crate::sim::rng::{sim_random, sim_random_range};

/// A simple bitset for tracking sector states.
///
/// Used to track which sectors are dirty, damaged, or under a targeted
/// injection. Implements a compact representation using u64 words.
#[derive(Debug, Clone)]
pub struct SectorBitSet {
    bits: Vec<u64>,
    len: usize,
}

impl SectorBitSet {
    /// Create a new bitset with capacity for the given number of sectors.
    ///
    /// All bits are initially unset (false).
    #[must_use]
    pub fn new(num_sectors: usize) -> Self {
        Self {
            bits: vec![0; num_sectors.div_ceil(64)],
            len: num_sectors,
        }
    }

    /// Set the bit for the given sector, if it is in range.
    pub fn set(&mut self, sector: usize) {
        if sector < self.len {
            self.bits[sector / 64] |= 1 << (sector % 64);
        }
    }

    /// Clear the bit for the given sector, if it is in range.
    pub fn clear(&mut self, sector: usize) {
        if sector < self.len {
            self.bits[sector / 64] &= !(1 << (sector % 64));
        }
    }

    /// Check if the bit for the given sector is set. Out-of-range sectors read
    /// as unset.
    #[must_use]
    pub fn is_set(&self, sector: usize) -> bool {
        sector < self.len && (self.bits[sector / 64] & (1 << (sector % 64))) != 0
    }

    /// Return the number of sectors this bitset can track.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Check if the bitset is empty (has zero capacity).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether any sector in the range is set.
    #[must_use]
    pub fn any_in(&self, sectors: Range<u64>) -> bool {
        sectors
            .map_while(|s| usize::try_from(s).ok())
            .any(|s| self.is_set(s))
    }

    /// Clear every bit.
    pub fn clear_all(&mut self) {
        self.bits.iter_mut().for_each(|word| *word = 0);
    }

    /// Resize, preserving the bits that still fit.
    fn resized(&self, new_len: usize) -> Self {
        let mut resized = Self::new(new_len);
        for sector in 0..self.len.min(new_len) {
            if self.is_set(sector) {
                resized.set(sector);
            }
        }
        resized
    }
}

/// Deterministic fill for never-written and lost sectors: zeros or per-sector
/// garbage, chosen once per file.
///
/// The garbage is a pure function of `(seed, sector)` — a keyed generator, not
/// a randomness source — so re-reading a lost sector returns the same bytes.
/// Zeros are the dangerous real-world case (SATA `RZAT`, `NVMe` `DLFEAT`,
/// unwritten extents): code that infers "never written" from "reads as zero"
/// must be driven red, so both fills have to happen.
#[derive(Debug, Clone, Copy)]
struct FillPattern {
    garbage: bool,
    seed: u64,
}

impl FillPattern {
    fn sector(self, sector: u64) -> [u8; SECTOR_SIZE] {
        let mut bytes = [0u8; SECTOR_SIZE];
        if self.garbage {
            let mix = self.seed.wrapping_add(sector);
            ChaCha8Rng::seed_from_u64(mix).fill(&mut bytes);
        }
        bytes
    }
}

/// The state one simulated file's bytes are in.
#[derive(Debug)]
pub struct FileImage {
    /// Durable bytes as of the last successful sync.
    committed: Vec<u8>,
    /// Bytes reads observe: committed plus everything written since.
    visible: Vec<u8>,
    /// Sectors modified since the last successful sync.
    dirty: SectorBitSet,
    /// Sectors a sync lied about: reported durable, left volatile.
    lied: SectorBitSet,
    /// Sectors with a latent read fault (deterministic corruption on read).
    faults: SectorBitSet,
    /// Targeted EIO injections.
    eio_read: SectorBitSet,
    eio_write: SectorBitSet,
    /// Per-sector `(CRC, byte length)` of the content the caller believes
    /// durable. A stamp exists only for sectors a sync claimed; a write drops
    /// it. The length is kept because the file's last sector is partial, and a
    /// sector that shrank away with a surviving truncation must not be
    /// mistaken for a damaged one.
    oracle: BTreeMap<u64, (u32, usize)>,
    fill: FillPattern,
}

impl FileImage {
    /// Create an image of `size` bytes whose never-written sectors read the
    /// fill pattern chosen for this file.
    ///
    /// `garbage_fill` picks the pattern (drawn by the caller so the image
    /// itself holds no randomness policy), `seed` keys it.
    #[must_use]
    pub fn new(size: u64, seed: u64, garbage_fill: bool) -> Self {
        let fill = FillPattern {
            garbage: garbage_fill,
            seed,
        };
        let mut image = Self {
            committed: Vec::new(),
            visible: Vec::new(),
            dirty: SectorBitSet::new(0),
            lied: SectorBitSet::new(0),
            faults: SectorBitSet::new(0),
            eio_read: SectorBitSet::new(0),
            eio_write: SectorBitSet::new(0),
            oracle: BTreeMap::new(),
            fill,
        };
        image.resize_visible(size);
        image.committed = image.visible.clone();
        // The initial image *is* the durable one.
        image.dirty.clear_all();
        image
    }

    /// Current (visible) size in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.visible.len() as u64
    }

    /// Size as of the last successful sync.
    #[must_use]
    pub fn durable_size(&self) -> u64 {
        self.committed.len() as u64
    }

    /// Number of sectors the visible image spans.
    #[must_use]
    pub fn sectors(&self) -> u64 {
        self.size().div_ceil(SECTOR_SIZE as u64)
    }

    /// Whether a read touching `sectors` is under a targeted EIO injection.
    #[must_use]
    pub fn read_fails(&self, sectors: Range<u64>) -> bool {
        self.eio_read.any_in(sectors)
    }

    /// Whether a write touching `sectors` is under a targeted EIO injection.
    #[must_use]
    pub fn write_fails(&self, sectors: Range<u64>) -> bool {
        self.eio_write.any_in(sectors)
    }

    /// Read `buf.len()` bytes from `offset`, applying latent corruption.
    ///
    /// # Errors
    ///
    /// Returns an error if the range runs past the end of the file.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let end = offset.checked_add(buf.len() as u64).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "offset overflow")
        })?;
        if end > self.size() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "read past end of file: offset={offset}, len={}, size={}",
                    buf.len(),
                    self.size()
                ),
            ));
        }
        let start = as_index(offset);
        buf.copy_from_slice(&self.visible[start..start + buf.len()]);

        for sector in super::faults::sector_range(offset, buf.len()) {
            let index = as_index(sector);
            if !self.faults.is_set(index) {
                continue;
            }
            let sector_start = index * SECTOR_SIZE;
            let overlap_start = sector_start.max(start);
            let overlap_end = (sector_start + SECTOR_SIZE).min(start + buf.len());
            if overlap_start >= overlap_end {
                continue;
            }
            corrupt_in_place(
                &self.visible[self.visible_bounds(sector)],
                &mut buf[overlap_start - start..overlap_end - start],
            );
        }
        Ok(())
    }

    /// Write `data` at `offset`, extending the file if needed.
    ///
    /// The bytes become visible immediately and durable only at the next
    /// successful [`sync`](Self::sync). The written sectors lose any latent
    /// fault, any lie, and any durability stamp: overwriting destroys the
    /// guarantee the previous content carried.
    pub fn write(&mut self, offset: u64, data: &[u8]) {
        let end = offset.saturating_add(data.len() as u64);
        if end > self.size() {
            self.resize_visible(end);
        }
        let start = as_index(offset);
        self.visible[start..start + data.len()].copy_from_slice(data);
        for sector in super::faults::sector_range(offset, data.len()) {
            let index = as_index(sector);
            self.dirty.set(index);
            self.faults.clear(index);
            self.lied.clear(index);
            self.oracle.remove(&sector);
        }
    }

    /// Resize the visible image; the new length is durable only at the next
    /// sync.
    pub fn set_len(&mut self, new_len: u64) {
        let old_sectors = self.sectors();
        self.resize_visible(new_len);
        // Sectors that fell off the end carry nothing forward. The length
        // change itself is unsynced metadata: the crash model resolves it
        // against the durable length.
        for sector in self.sectors()..old_sectors {
            self.oracle.remove(&sector);
        }
    }

    /// Make every visible byte durable: the barrier.
    ///
    /// Returns the sectors this sync *lied* about — reported durable while
    /// leaving them volatile (the opt-in barrier-violation family).
    pub fn sync(
        &mut self,
        config: &StorageConfiguration,
        eligible: &dyn Fn(u64) -> bool,
    ) -> Vec<u64> {
        self.committed.resize(self.visible.len(), 0);
        let lie_probability = config.barrier_violation_probability;
        let mut lied = Vec::new();
        for sector in self.dirty_sectors() {
            let index = as_index(sector);
            let lie =
                lie_probability > 0.0 && sim_random::<f64>() < lie_probability && eligible(sector);
            let range = self.visible_bounds(sector);
            self.oracle.insert(
                sector,
                (crc32c::crc32c(&self.visible[range.clone()]), range.len()),
            );
            if lie {
                assert_reachable!("disk: sync lied, a synced write was left volatile");
                self.lied.set(index);
                lied.push(sector);
            } else {
                let bytes = self.visible[range.clone()].to_vec();
                self.committed[range].copy_from_slice(&bytes);
                self.dirty.clear(index);
                self.lied.clear(index);
            }
        }
        lied
    }

    /// Resolve a crash: every sector dirty at this instant lands old, new,
    /// lost, damaged, or shorn, independently.
    ///
    /// # Panics
    ///
    /// Panics when a sector a sync reported durable changed across the crash
    /// while the barrier-violation family is not armed — that is a simulator
    /// bug, never legal disk behaviour.
    pub fn crash(
        &mut self,
        path: &str,
        config: &StorageConfiguration,
        barrier_violation_armed: bool,
        eligible: &dyn Fn(u64) -> bool,
    ) -> FileCrashReport {
        let mut report = FileCrashReport {
            path: path.to_string(),
            ..FileCrashReport::default()
        };

        // The unsynced length change resolves first: the new length either
        // survives or the file reverts to its last durable length.
        if self.visible.len() != self.committed.len() {
            if sim_random::<f64>() < config.length_survives_crash_probability {
                assert_reachable!("disk crash: an unsynced length change survived");
                let grown_from = self.committed_sectors();
                self.committed.resize(self.visible.len(), 0);
                for sector in grown_from..self.sectors() {
                    let range = self.committed_bounds(sector);
                    let bytes = self.fill.sector(sector);
                    self.committed[range.clone()].copy_from_slice(&bytes[..range.len()]);
                }
            } else {
                assert_reachable!("disk crash: an unsynced length change reverted");
                let durable = self.committed.len();
                self.visible.truncate(durable);
                self.resize_bitsets(self.sectors());
                report.length_reverted = true;
            }
        }

        let dirty = self.dirty_sectors();
        let lied: Vec<u64> = dirty
            .iter()
            .copied()
            .filter(|s| self.lied.is_set(as_index(*s)))
            .collect();

        if sim_random::<f64>() < config.clean_crash_probability {
            assert_reachable!("disk crash: clean crash preserved every unsynced write");
            report.clean = true;
            for sector in dirty {
                self.commit_sector(sector);
            }
        } else {
            let window = correlated_window(&dirty, config);
            let mut correlated_fired = false;
            for sector in dirty {
                let in_window = window.as_ref().is_some_and(|w| w.contains(&sector));
                correlated_fired |= in_window;
                let index = as_index(sector);
                let outcome =
                    choose_outcome(config, sector, self.lied.is_set(index), in_window, eligible);
                self.apply_outcome(sector, outcome);
                note_outcome_reachable(outcome, self.fill.garbage);
                report.resolutions.push(SectorResolution {
                    sector,
                    outcome,
                    correlated: in_window,
                });
            }
            if correlated_fired {
                assert_reachable!("disk crash: correlated rollback of a sector run");
            }
        }

        // Converge: the surviving durable image is what the reboot sees.
        self.visible.clear();
        self.visible.extend_from_slice(&self.committed);
        let sectors = self.sectors();
        self.resize_bitsets(sectors);
        self.dirty.clear_all();
        self.lied.clear_all();

        report.lost_synced = self.oracle_sweep(path, &lied, barrier_violation_armed);
        report
    }

    /// Plant a latent read fault on `sectors`: reads return deterministically
    /// corrupted bytes until the sectors are rewritten.
    pub fn corrupt(&mut self, sectors: Range<u64>) {
        for sector in sectors {
            self.faults.set(as_index(sector));
        }
    }

    /// Whether a sector currently carries a latent read fault.
    #[must_use]
    pub fn is_corrupt(&self, sector: u64) -> bool {
        self.faults.is_set(as_index(sector))
    }

    /// Make reads and/or writes touching `sectors` fail with an I/O error
    /// until cleared.
    pub fn fail_with_eio(&mut self, sectors: Range<u64>, target: EioTarget) {
        for sector in sectors {
            let index = as_index(sector);
            if matches!(target, EioTarget::Read | EioTarget::ReadWrite) {
                self.eio_read.set(index);
            }
            if matches!(target, EioTarget::Write | EioTarget::ReadWrite) {
                self.eio_write.set(index);
            }
        }
    }

    /// Clear targeted EIO injections.
    pub fn clear_eio(&mut self, target: EioTarget) {
        if matches!(target, EioTarget::Read | EioTarget::ReadWrite) {
            self.eio_read.clear_all();
        }
        if matches!(target, EioTarget::Write | EioTarget::ReadWrite) {
            self.eio_write.clear_all();
        }
    }

    /// Mutate a *committed* sector out of band, bypassing the crash model.
    ///
    /// A simulator bug on purpose: the next crash must fail loudly unless the
    /// barrier-violation family is armed. Exists so the oracle itself can be
    /// tested.
    pub fn corrupt_committed_out_of_band(&mut self, sector: u64) {
        let range = self.committed_bounds(sector);
        if !range.is_empty() {
            self.committed[range.start] ^= 0xFF;
        }
    }

    fn dirty_sectors(&self) -> Vec<u64> {
        (0..self.sectors())
            .filter(|s| self.dirty.is_set(as_index(*s)))
            .collect()
    }

    fn committed_sectors(&self) -> u64 {
        (self.committed.len() as u64).div_ceil(SECTOR_SIZE as u64)
    }

    /// Byte range of one sector inside both images: a file's last sector is
    /// partial, and the two images differ in length until a crash resolves the
    /// difference. Empty when the sector lies outside one of them.
    fn shared_bounds(&self, sector: u64) -> Range<usize> {
        clamp_sector(sector, self.visible.len().min(self.committed.len()))
    }

    /// Byte range of one sector inside the durable image alone.
    fn committed_bounds(&self, sector: u64) -> Range<usize> {
        clamp_sector(sector, self.committed.len())
    }

    /// Byte range of one sector inside the visible image alone.
    fn visible_bounds(&self, sector: u64) -> Range<usize> {
        clamp_sector(sector, self.visible.len())
    }

    /// Grow or shrink the visible image, filling any extension with the
    /// file's fill pattern.
    fn resize_visible(&mut self, new_len: u64) {
        let old_len = self.visible.len();
        let new_len = as_index(new_len);
        self.visible.resize(new_len, 0);
        self.resize_bitsets(new_len.div_ceil(SECTOR_SIZE) as u64);
        if new_len <= old_len {
            return;
        }
        for sector in (old_len / SECTOR_SIZE)..new_len.div_ceil(SECTOR_SIZE) {
            let start = (sector * SECTOR_SIZE).max(old_len);
            let end = ((sector + 1) * SECTOR_SIZE).min(new_len);
            if start >= end {
                continue;
            }
            let bytes = self.fill.sector(sector as u64);
            let offset_in_sector = start - sector * SECTOR_SIZE;
            self.visible[start..end]
                .copy_from_slice(&bytes[offset_in_sector..offset_in_sector + (end - start)]);
            // Newly addressable bytes are not durable until the next sync,
            // so they resolve through the crash model like any other write.
            self.dirty.set(sector);
        }
    }

    fn resize_bitsets(&mut self, sectors: u64) {
        let len = as_index(sectors);
        self.dirty = self.dirty.resized(len);
        self.lied = self.lied.resized(len);
        self.faults = self.faults.resized(len);
        self.eio_read = self.eio_read.resized(len);
        self.eio_write = self.eio_write.resized(len);
    }

    /// Commit one dirty sector honestly: the visible bytes become durable.
    fn commit_sector(&mut self, sector: u64) {
        let range = self.shared_bounds(sector);
        if !range.is_empty() {
            let bytes = self.visible[range.clone()].to_vec();
            self.committed[range].copy_from_slice(&bytes);
        }
        let index = as_index(sector);
        self.dirty.clear(index);
        self.lied.clear(index);
    }

    /// Materialize one sector's crash resolution into the committed image.
    fn apply_outcome(&mut self, sector: u64, outcome: CrashOutcome) {
        let index = as_index(sector);
        match outcome {
            CrashOutcome::KeptOld => {
                self.dirty.clear(index);
                self.lied.clear(index);
            }
            CrashOutcome::KeptNew => self.commit_sector(sector),
            CrashOutcome::LatentFault => {
                self.commit_sector(sector);
                self.faults.set(index);
            }
            CrashOutcome::Lost => {
                let range = self.committed_bounds(sector);
                if !range.is_empty() {
                    let bytes = self.fill.sector(sector);
                    self.committed[range.clone()].copy_from_slice(&bytes[..range.len()]);
                }
                self.dirty.clear(index);
                self.lied.clear(index);
            }
            CrashOutcome::Shorn => {
                let range = self.shared_bounds(sector);
                if range.len() > 1 {
                    let split = sim_random_range(1..range.len());
                    let prefix_new = sim_random::<bool>();
                    let kept = if prefix_new {
                        range.start..range.start + split
                    } else {
                        range.start + split..range.end
                    };
                    let bytes = self.visible[kept.clone()].to_vec();
                    self.committed[kept].copy_from_slice(&bytes);
                }
                self.dirty.clear(index);
                self.lied.clear(index);
            }
        }
    }

    /// Verify that every sector a sync reported durable still holds what the
    /// caller was told, and report the ones a lying sync lost.
    fn oracle_sweep(&mut self, path: &str, lied: &[u64], armed: bool) -> Vec<u64> {
        let mut lost = Vec::new();
        let stamps: Vec<(u64, (u32, usize))> =
            self.oracle.iter().map(|(s, stamp)| (*s, *stamp)).collect();
        for (sector, (crc, stamped_len)) in stamps {
            let range = self.committed_bounds(sector);
            if range.len() != stamped_len {
                // The sector shrank away with a surviving truncation; the
                // length resolution accounted for it, and comparing a
                // different number of bytes would be meaningless.
                self.oracle.remove(&sector);
                continue;
            }
            if crc32c::crc32c(&self.committed[range]) == crc {
                continue;
            }
            assert!(
                armed && lied.contains(&sector),
                "moonpool sim bug: sector {sector} of '{path}' was reported durable by a sync \
                 but changed across a crash without the barrier-violation family armed"
            );
            lost.push(sector);
        }
        for sector in &lost {
            assert_reachable!("disk crash: lost a synced write (barrier violation)");
            self.oracle.remove(sector);
        }
        lost
    }
}

/// Pick a contiguous run of dirty sectors to roll back together — correlated
/// erase-block damage (Zheng, FAST'13).
fn correlated_window(dirty: &[u64], config: &StorageConfiguration) -> Option<Range<u64>> {
    if dirty.is_empty()
        || config.correlated_rollback_probability <= 0.0
        || sim_random::<f64>() >= config.correlated_rollback_probability
    {
        return None;
    }
    let anchor = dirty[sim_random_range(0..dirty.len())];
    let run = sim_random_range(1..config.correlated_rollback_max_run.max(1) + 1);
    Some(anchor..anchor.saturating_add(run))
}

/// A `u64` offset or index as a slice index.
///
/// Saturating rather than panicking: on a 64-bit target it is the identity,
/// and on a narrower one a value this large cannot address anything in the
/// image, so every use is behind a bounds check that rejects it.
fn as_index(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Byte range of `sector` clamped to an image of `limit` bytes; empty when the
/// sector lies past the end.
fn clamp_sector(sector: u64, limit: usize) -> Range<usize> {
    let start = as_index(sector.saturating_mul(SECTOR_SIZE as u64));
    if start >= limit {
        return 0..0;
    }
    start..(start + SECTOR_SIZE).min(limit)
}

/// Pick the resolution shape for one dirty sector.
///
/// Damaging shapes are gated by the eligibility mask: an ineligible sector
/// falls back to a plain rollback. Sectors a sync lied about always roll back
/// — the whole point of the lie is that the write was never durable.
fn choose_outcome(
    config: &StorageConfiguration,
    sector: u64,
    lied: bool,
    in_window: bool,
    eligible: &dyn Fn(u64) -> bool,
) -> CrashOutcome {
    if in_window || lied {
        return CrashOutcome::KeptOld;
    }
    let roll = sim_random::<f64>();
    let allowed = eligible(sector);
    let lost_at = config.crash_lost_probability;
    let latent_at = lost_at + config.crash_latent_fault_probability;
    let shorn_at = latent_at + config.shorn_write_probability;
    let damaging = |outcome| {
        if allowed {
            outcome
        } else {
            CrashOutcome::KeptOld
        }
    };
    if roll < lost_at {
        return damaging(CrashOutcome::Lost);
    }
    if roll < latent_at {
        return damaging(CrashOutcome::LatentFault);
    }
    if roll < shorn_at {
        return damaging(CrashOutcome::Shorn);
    }
    let remaining = (1.0 - shorn_at).max(f64::EPSILON);
    if (roll - shorn_at) / remaining < 0.5 {
        CrashOutcome::KeptOld
    } else {
        CrashOutcome::KeptNew
    }
}

fn note_outcome_reachable(outcome: CrashOutcome, garbage_fill: bool) {
    match outcome {
        CrashOutcome::KeptOld => {
            assert_reachable!("disk crash: sector rolled back to its durable contents");
        }
        CrashOutcome::KeptNew => {
            assert_reachable!("disk crash: sector kept its unsynced contents");
        }
        CrashOutcome::Lost => {
            if garbage_fill {
                assert_reachable!("disk crash: sector lost to garbage fill");
            } else {
                assert_reachable!("disk crash: sector lost to zeros");
            }
        }
        CrashOutcome::LatentFault => {
            assert_reachable!("disk crash: sector left with a latent read fault");
        }
        CrashOutcome::Shorn => {
            assert_reachable!("disk crash: sector shorn sub-sector");
        }
    }
}

/// Deterministically corrupt a read buffer, seeded from the pristine content
/// so retries observe the identical corruption.
fn corrupt_in_place(pristine: &[u8], buf: &mut [u8]) {
    if buf.is_empty() {
        return;
    }
    let mut seed_bytes = [0u8; 8];
    let n = pristine.len().min(8);
    seed_bytes[..n].copy_from_slice(&pristine[..n]);
    let mut rng = ChaCha8Rng::seed_from_u64(u64::from_le_bytes(seed_bytes));
    let byte = rng.random_range(0..buf.len());
    let bit = rng.random_range(0..8u8);
    buf[byte] ^= 1 << bit;
}
