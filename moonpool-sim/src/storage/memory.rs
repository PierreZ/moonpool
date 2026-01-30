//! In-memory storage simulation with deterministic fault injection.
//!
//! This module provides `InMemoryStorage`, a low-level backing store for simulated
//! files that follows TigerBeetle's pristine memory + fault bitmap pattern.
//!
//! ## Key Design Insight
//!
//! Pristine data stays clean. Faults are applied on READ, not stored in data.
//! This allows toggling faults without data loss, making it easy to simulate
//! various storage failure scenarios.
//!
//! ## TigerBeetle References
//!
//! - Fault bitmap pattern: storage simulation
//! - Misdirected writes: lines 476-480
//! - Overlay system for read-time fault injection

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::io;

/// Size of a disk sector in bytes.
///
/// Most storage devices operate in 512-byte sectors. Operations that don't
/// align to sector boundaries may exhibit different behavior under faults.
pub const SECTOR_SIZE: usize = 512;

/// Maximum number of overlays for misdirected write simulation.
///
/// TigerBeetle uses 2 overlays per misdirected write:
/// 1. Original data at intended target (so reads see old data)
/// 2. New data at mistaken target (so reads see wrong data there)
const MAX_OVERLAYS: usize = 2;

/// A simple bitset for tracking sector states.
///
/// Used to track which sectors have been written and which have faults.
/// Implements a compact representation using u64 words.
#[derive(Debug, Clone)]
pub struct SectorBitSet {
    bits: Vec<u64>,
    len: usize,
}

impl SectorBitSet {
    /// Create a new bitset with capacity for the given number of sectors.
    ///
    /// All bits are initially unset (false).
    pub fn new(num_sectors: usize) -> Self {
        let num_words = num_sectors.div_ceil(64);
        Self {
            bits: vec![0; num_words],
            len: num_sectors,
        }
    }

    /// Set the bit for the given sector.
    ///
    /// # Panics
    ///
    /// Panics if `sector` is out of bounds.
    pub fn set(&mut self, sector: usize) {
        assert!(sector < self.len, "sector index out of bounds");
        let word = sector / 64;
        let bit = sector % 64;
        self.bits[word] |= 1 << bit;
    }

    /// Clear the bit for the given sector.
    ///
    /// # Panics
    ///
    /// Panics if `sector` is out of bounds.
    pub fn clear(&mut self, sector: usize) {
        assert!(sector < self.len, "sector index out of bounds");
        let word = sector / 64;
        let bit = sector % 64;
        self.bits[word] &= !(1 << bit);
    }

    /// Check if the bit for the given sector is set.
    ///
    /// # Panics
    ///
    /// Panics if `sector` is out of bounds.
    pub fn is_set(&self, sector: usize) -> bool {
        assert!(sector < self.len, "sector index out of bounds");
        let word = sector / 64;
        let bit = sector % 64;
        (self.bits[word] & (1 << bit)) != 0
    }

    /// Return the number of sectors this bitset can track.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Check if the bitset is empty (has zero capacity).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Overlay for misdirected write simulation.
///
/// When a misdirected write occurs, we need to show different data at
/// specific offsets during reads without corrupting the pristine data.
#[derive(Debug, Clone)]
struct WriteOverlay {
    /// Starting offset for this overlay
    offset: u64,
    /// Size of the overlay data
    size: u32,
    /// The data to show instead of pristine data
    data: Vec<u8>,
    /// Whether this overlay is currently active
    active: bool,
}

/// Pending write waiting to be synced.
///
/// Used for crash simulation - pending writes may be lost or partially
/// written if a crash occurs before sync.
#[derive(Debug, Clone)]
struct PendingWrite {
    /// Starting offset of the write
    offset: u64,
    /// Data that was written
    data: Vec<u8>,
    /// If true, this write will be lost on crash (phantom write)
    is_phantom: bool,
}

/// In-memory storage with deterministic fault injection.
///
/// This struct represents a simulated storage device that can inject
/// various faults at read time while keeping pristine data intact.
///
/// # Design
///
/// - `data`: Pristine storage contents
/// - `written`: Tracks which sectors have been written (unwritten sectors return random data)
/// - `faults`: Tracks which sectors have faults (faulted sectors return corrupted data)
/// - `overlays`: Temporary data overlays for misdirected write simulation
/// - `pending_writes`: Writes that haven't been synced yet (may be lost on crash)
///
/// # Example
///
/// ```ignore
/// use moonpool_sim::storage::memory::{InMemoryStorage, SECTOR_SIZE};
///
/// let mut storage = InMemoryStorage::new(4096, 42);
/// storage.write(0, b"Hello, World!", true)?;
///
/// let mut buf = vec![0u8; 13];
/// storage.read(0, &mut buf)?;
/// assert_eq!(&buf, b"Hello, World!");
/// ```
#[derive(Debug)]
pub struct InMemoryStorage {
    /// Pristine data - faults are applied on read, not stored here
    data: Vec<u8>,
    /// Which sectors have been written
    written: SectorBitSet,
    /// Which sectors have faults (corruption applied on read)
    faults: SectorBitSet,
    /// Overlays for misdirected write simulation
    overlays: [Option<WriteOverlay>; MAX_OVERLAYS],
    /// Pending writes that haven't been synced
    pending_writes: Vec<PendingWrite>,
    /// Total size of the storage in bytes
    size: u64,
    /// Seed for deterministic random generation
    seed: u64,
}

impl InMemoryStorage {
    /// Create a new in-memory storage with the given size and seed.
    ///
    /// # Arguments
    ///
    /// * `size` - Total size of the storage in bytes
    /// * `seed` - Seed for deterministic random generation (used for unwritten sector fill)
    pub fn new(size: u64, seed: u64) -> Self {
        let num_sectors = (size as usize).div_ceil(SECTOR_SIZE);
        Self {
            data: vec![0; size as usize],
            written: SectorBitSet::new(num_sectors),
            faults: SectorBitSet::new(num_sectors),
            overlays: [const { None }; MAX_OVERLAYS],
            pending_writes: Vec::new(),
            size,
            seed,
        }
    }

    /// Get the total size of the storage in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Get the number of sectors in this storage.
    pub fn num_sectors(&self) -> usize {
        self.written.len()
    }

    /// Read data from storage, applying faults as needed.
    ///
    /// # Fault Application
    ///
    /// 1. Unwritten sectors are filled with deterministic random data
    /// 2. Faulted sectors have deterministic corruption applied
    /// 3. Active overlays are applied on top
    ///
    /// # Arguments
    ///
    /// * `offset` - Starting byte offset
    /// * `buf` - Buffer to read into
    ///
    /// # Errors
    ///
    /// Returns an error if the read would go past the end of storage.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        // Bounds check
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"))?;

        if end > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "read past end of storage: offset={}, len={}, size={}",
                    offset,
                    buf.len(),
                    self.size
                ),
            ));
        }

        // Copy pristine data to buffer
        let offset_usize = offset as usize;
        buf.copy_from_slice(&self.data[offset_usize..offset_usize + buf.len()]);

        // Apply sector-level effects
        let start_sector = offset_usize / SECTOR_SIZE;
        let end_sector = (offset_usize + buf.len()).div_ceil(SECTOR_SIZE);

        for sector in start_sector..end_sector {
            if sector >= self.written.len() {
                break;
            }

            // Calculate which part of the buffer corresponds to this sector
            let sector_start = sector * SECTOR_SIZE;
            let sector_end = sector_start + SECTOR_SIZE;

            let buf_start = sector_start.saturating_sub(offset_usize);
            let buf_end = (sector_end.saturating_sub(offset_usize)).min(buf.len());

            if buf_start >= buf_end {
                continue;
            }

            let sector_buf = &mut buf[buf_start..buf_end];

            // If sector not written, fill with deterministic random
            if !self.written.is_set(sector) {
                self.fill_unwritten_sector(sector, sector_buf, sector_start, offset_usize);
            }

            // If sector has fault, apply corruption
            if self.faults.is_set(sector) {
                self.apply_corruption(sector, sector_buf);
            }
        }

        // Apply active overlays
        self.apply_overlays(offset, buf);

        Ok(())
    }

    /// Fill unwritten sector data with deterministic random bytes.
    fn fill_unwritten_sector(
        &self,
        sector: usize,
        buf: &mut [u8],
        sector_start: usize,
        read_offset: usize,
    ) {
        // Use seed + sector as RNG seed for deterministic fill
        let mut rng = ChaCha8Rng::seed_from_u64(self.seed.wrapping_add(sector as u64));

        // Generate full sector of random data
        let mut sector_data = [0u8; SECTOR_SIZE];
        rng.fill(&mut sector_data);

        // Copy relevant portion to buffer
        let offset_in_sector = read_offset.saturating_sub(sector_start);
        let copy_start = offset_in_sector.min(SECTOR_SIZE);
        let copy_len = buf.len().min(SECTOR_SIZE - copy_start);

        buf[..copy_len].copy_from_slice(&sector_data[copy_start..copy_start + copy_len]);
    }

    /// Apply deterministic corruption to a sector.
    ///
    /// Uses the pristine bytes as seed so retries don't help - the same
    /// corruption will occur each time.
    ///
    /// TigerBeetle reference: lines 476-480
    fn apply_corruption(&self, sector: usize, buf: &mut [u8]) {
        if buf.is_empty() {
            return;
        }

        // Use pristine bytes as seed so retries don't help
        let sector_start = sector * SECTOR_SIZE;
        let seed_bytes = if sector_start + 8 <= self.data.len() {
            self.data[sector_start..sector_start + 8]
                .try_into()
                .unwrap_or([0u8; 8])
        } else {
            [0u8; 8]
        };
        let seed = u64::from_le_bytes(seed_bytes);

        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let byte_idx = rng.random_range(0..buf.len());
        let bit_idx = rng.random_range(0..8u8);
        buf[byte_idx] ^= 1 << bit_idx;
    }

    /// Apply active overlays to the read buffer.
    fn apply_overlays(&self, offset: u64, buf: &mut [u8]) {
        for overlay in self.overlays.iter().flatten() {
            if !overlay.active {
                continue;
            }

            // Check if overlay intersects with read range
            let overlay_end = overlay.offset + overlay.size as u64;
            let read_end = offset + buf.len() as u64;

            if overlay.offset >= read_end || overlay_end <= offset {
                continue;
            }

            // Calculate intersection
            let intersect_start = overlay.offset.max(offset);
            let intersect_end = overlay_end.min(read_end);

            let buf_offset = (intersect_start - offset) as usize;
            let overlay_offset = (intersect_start - overlay.offset) as usize;
            let copy_len = (intersect_end - intersect_start) as usize;

            buf[buf_offset..buf_offset + copy_len]
                .copy_from_slice(&overlay.data[overlay_offset..overlay_offset + copy_len]);
        }
    }

    /// Write data to storage.
    ///
    /// # Arguments
    ///
    /// * `offset` - Starting byte offset
    /// * `data` - Data to write
    /// * `is_synced` - If false, write is added to pending writes (may be lost on crash)
    ///
    /// # Errors
    ///
    /// Returns an error if the write would go past the end of storage.
    pub fn write(&mut self, offset: u64, data: &[u8], is_synced: bool) -> io::Result<()> {
        // Bounds check
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"))?;

        if end > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "write past end of storage: offset={}, len={}, size={}",
                    offset,
                    data.len(),
                    self.size
                ),
            ));
        }

        let offset_usize = offset as usize;

        // Mark sectors as written and clear faults
        let start_sector = offset_usize / SECTOR_SIZE;
        let end_sector = (offset_usize + data.len()).div_ceil(SECTOR_SIZE);

        for sector in start_sector..end_sector {
            if sector < self.written.len() {
                self.written.set(sector);
                self.faults.clear(sector);
            }
        }

        // Copy data to pristine storage
        self.data[offset_usize..offset_usize + data.len()].copy_from_slice(data);

        // If not synced, add to pending writes
        if !is_synced {
            self.pending_writes.push(PendingWrite {
                offset,
                data: data.to_vec(),
                is_phantom: false,
            });
        }

        Ok(())
    }

    /// Sync all pending writes, making them durable.
    ///
    /// After sync, pending writes are cleared and won't be affected by crash simulation.
    pub fn sync(&mut self) {
        self.pending_writes.clear();
    }

    /// Apply a misdirected write.
    ///
    /// Simulates a write that lands at the wrong location. Uses the TigerBeetle
    /// 2-overlay pattern:
    /// 1. Overlay 1: intended target shows old data on read
    /// 2. Overlay 2: mistaken target shows new data on read
    /// 3. Pristine memory is updated at intended target
    ///
    /// # Arguments
    ///
    /// * `intended_offset` - Where the write should have gone
    /// * `mistaken_offset` - Where the write actually went
    /// * `data` - The data that was written
    ///
    /// # Errors
    ///
    /// Returns an error if either offset would go past the end of storage.
    pub fn apply_misdirected_write(
        &mut self,
        intended_offset: u64,
        mistaken_offset: u64,
        data: &[u8],
    ) -> io::Result<()> {
        // Bounds checks
        let intended_end = intended_offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "intended offset overflow")
            })?;

        let mistaken_end = mistaken_offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "mistaken offset overflow")
            })?;

        if intended_end > self.size || mistaken_end > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "misdirected write past end of storage",
            ));
        }

        // Save old data at intended target
        let intended_usize = intended_offset as usize;
        let old_data = self.data[intended_usize..intended_usize + data.len()].to_vec();

        // Overlay 1: intended target shows old data on read
        self.overlays[0] = Some(WriteOverlay {
            offset: intended_offset,
            size: data.len() as u32,
            data: old_data,
            active: true,
        });

        // Overlay 2: mistaken target shows new data on read
        self.overlays[1] = Some(WriteOverlay {
            offset: mistaken_offset,
            size: data.len() as u32,
            data: data.to_vec(),
            active: true,
        });

        // Update pristine memory at intended target (the physical write happened)
        self.data[intended_usize..intended_usize + data.len()].copy_from_slice(data);

        // Mark sectors as written at intended target
        let start_sector = intended_usize / SECTOR_SIZE;
        let end_sector = (intended_usize + data.len()).div_ceil(SECTOR_SIZE);
        for sector in start_sector..end_sector {
            if sector < self.written.len() {
                self.written.set(sector);
            }
        }

        Ok(())
    }

    /// Clear all active overlays.
    ///
    /// Call this to reset the misdirection state.
    pub fn clear_overlays(&mut self) {
        for overlay in &mut self.overlays {
            *overlay = None;
        }
    }

    /// Read data from a misdirected location.
    ///
    /// Instead of reading from `offset`, reads from a different location
    /// chosen deterministically based on the seed. The caller is responsible
    /// for deciding when to call this vs regular `read()`.
    ///
    /// # Arguments
    ///
    /// * `offset` - The intended read offset (will read from somewhere else)
    /// * `buf` - Buffer to read into
    ///
    /// # Errors
    ///
    /// Returns an error if the read would go past the end of storage.
    pub fn read_misdirected(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }

        // Pick a random different offset using the original offset as seed
        let mut rng = ChaCha8Rng::seed_from_u64(self.seed.wrapping_add(offset));

        // Calculate valid range for misdirected read
        let max_offset = self.size.saturating_sub(buf.len() as u64);
        if max_offset == 0 {
            // Buffer is larger than storage, just read from 0
            return self.read(0, buf);
        }

        let mut misdirected_offset = rng.random_range(0..max_offset);

        // Ensure we don't accidentally read from the intended location
        if misdirected_offset == offset {
            misdirected_offset = (misdirected_offset + SECTOR_SIZE as u64) % max_offset;
        }

        self.read(misdirected_offset, buf)
    }

    /// Record a phantom write.
    ///
    /// A phantom write appears to succeed but the data is never actually
    /// persisted. The data is added to pending writes with `is_phantom: true`,
    /// meaning it will be lost on crash without corrupting other data.
    ///
    /// # Arguments
    ///
    /// * `offset` - Starting byte offset
    /// * `data` - Data that "appeared" to be written
    pub fn record_phantom_write(&mut self, offset: u64, data: &[u8]) {
        self.pending_writes.push(PendingWrite {
            offset,
            data: data.to_vec(),
            is_phantom: true,
        });
        // Note: We do NOT update pristine data - the write didn't actually happen
    }

    /// Apply crash simulation to pending writes.
    ///
    /// For each pending non-phantom write, there's a chance that a sector
    /// in the write range gets marked as faulted (simulating a torn write).
    /// Phantom writes simply disappear.
    ///
    /// # Arguments
    ///
    /// * `crash_fault_probability` - Probability [0.0, 1.0] that each pending
    ///   write experiences a crash fault
    pub fn apply_crash(&mut self, crash_fault_probability: f64) {
        let mut rng = ChaCha8Rng::seed_from_u64(self.seed);

        for pending in &self.pending_writes {
            if pending.is_phantom {
                // Phantom writes just disappear - nothing to do
                continue;
            }

            // Check if this write experiences a crash fault
            if rng.random::<f64>() >= crash_fault_probability {
                continue;
            }

            // Pick a random sector in the write range to fault
            let offset_usize = pending.offset as usize;
            let start_sector = offset_usize / SECTOR_SIZE;
            let end_sector = (offset_usize + pending.data.len()).div_ceil(SECTOR_SIZE);

            if start_sector < end_sector && end_sector <= self.faults.len() {
                let faulted_sector = rng.random_range(start_sector..end_sector);
                self.faults.set(faulted_sector);
            }
        }

        // Clear all pending writes
        self.pending_writes.clear();
    }

    /// Manually set a sector as faulted.
    ///
    /// Useful for testing specific fault scenarios.
    pub fn set_fault(&mut self, sector: usize) {
        if sector < self.faults.len() {
            self.faults.set(sector);
        }
    }

    /// Manually clear a sector's fault.
    pub fn clear_fault(&mut self, sector: usize) {
        if sector < self.faults.len() {
            self.faults.clear(sector);
        }
    }

    /// Check if a sector has a fault.
    pub fn has_fault(&self, sector: usize) -> bool {
        sector < self.faults.len() && self.faults.is_set(sector)
    }

    /// Check if a sector has been written.
    pub fn is_written(&self, sector: usize) -> bool {
        sector < self.written.len() && self.written.is_set(sector)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_write_read() {
        let mut storage = InMemoryStorage::new(4096, 42);

        // Write data
        let data = b"Hello, World!";
        storage.write(0, data, true).expect("write failed");

        // Read it back
        let mut buf = vec![0u8; data.len()];
        storage.read(0, &mut buf).expect("read failed");

        assert_eq!(&buf, data);
    }

    #[test]
    fn test_unwritten_sector_deterministic() {
        let storage1 = InMemoryStorage::new(4096, 42);
        let storage2 = InMemoryStorage::new(4096, 42);

        // Read from unwritten sector
        let mut buf1 = vec![0u8; SECTOR_SIZE];
        let mut buf2 = vec![0u8; SECTOR_SIZE];

        storage1.read(0, &mut buf1).expect("read1 failed");
        storage2.read(0, &mut buf2).expect("read2 failed");

        // Same seed should produce same random fill
        assert_eq!(buf1, buf2);

        // Different seed should produce different fill
        let storage3 = InMemoryStorage::new(4096, 99);
        let mut buf3 = vec![0u8; SECTOR_SIZE];
        storage3.read(0, &mut buf3).expect("read3 failed");

        assert_ne!(buf1, buf3);
    }

    #[test]
    fn test_fault_corruption() {
        let mut storage = InMemoryStorage::new(4096, 42);

        // Write data
        let data = vec![0xAA; SECTOR_SIZE];
        storage.write(0, &data, true).expect("write failed");

        // Read without fault
        let mut buf_clean = vec![0u8; SECTOR_SIZE];
        storage.read(0, &mut buf_clean).expect("read failed");
        assert_eq!(buf_clean, data);

        // Set fault and read again
        storage.set_fault(0);
        let mut buf_faulted = vec![0u8; SECTOR_SIZE];
        storage.read(0, &mut buf_faulted).expect("read failed");

        // Data should be corrupted (exactly one bit flipped)
        assert_ne!(buf_faulted, data);

        // Count bit differences
        let bit_diffs: u32 = buf_clean
            .iter()
            .zip(buf_faulted.iter())
            .map(|(a, b)| (*a ^ *b).count_ones())
            .sum();
        assert_eq!(bit_diffs, 1, "Expected exactly one bit flip");
    }

    #[test]
    fn test_corruption_determinism() {
        let mut storage = InMemoryStorage::new(4096, 42);

        // Write data
        let data = vec![0xAA; SECTOR_SIZE];
        storage.write(0, &data, true).expect("write failed");
        storage.set_fault(0);

        // Read multiple times
        let mut buf1 = vec![0u8; SECTOR_SIZE];
        let mut buf2 = vec![0u8; SECTOR_SIZE];
        storage.read(0, &mut buf1).expect("read1 failed");
        storage.read(0, &mut buf2).expect("read2 failed");

        // Same fault should produce same corruption
        assert_eq!(buf1, buf2);
    }

    #[test]
    fn test_misdirected_write() {
        let mut storage = InMemoryStorage::new(4096, 42);

        // Write initial data at both locations
        let original_intended = vec![0x11; SECTOR_SIZE];
        let original_mistaken = vec![0x22; SECTOR_SIZE];
        storage
            .write(0, &original_intended, true)
            .expect("write1 failed");
        storage
            .write(SECTOR_SIZE as u64, &original_mistaken, true)
            .expect("write2 failed");

        // Apply misdirected write: intended=0, mistaken=SECTOR_SIZE
        let new_data = vec![0xFF; SECTOR_SIZE];
        storage
            .apply_misdirected_write(0, SECTOR_SIZE as u64, &new_data)
            .expect("misdirect failed");

        // Read from intended location - should see old data (overlay)
        let mut buf_intended = vec![0u8; SECTOR_SIZE];
        storage.read(0, &mut buf_intended).expect("read failed");
        assert_eq!(buf_intended, original_intended);

        // Read from mistaken location - should see new data (overlay)
        let mut buf_mistaken = vec![0u8; SECTOR_SIZE];
        storage
            .read(SECTOR_SIZE as u64, &mut buf_mistaken)
            .expect("read failed");
        assert_eq!(buf_mistaken, new_data);

        // Clear overlays and verify pristine state
        storage.clear_overlays();
        storage.read(0, &mut buf_intended).expect("read failed");
        assert_eq!(buf_intended, new_data); // Pristine was updated
    }

    #[test]
    fn test_phantom_write_lost_on_crash() {
        let mut storage = InMemoryStorage::new(4096, 42);

        // Write real data
        let real_data = vec![0x11; SECTOR_SIZE];
        storage.write(0, &real_data, true).expect("write failed");

        // Record phantom write (different data)
        let phantom_data = vec![0xFF; SECTOR_SIZE];
        storage.record_phantom_write(0, &phantom_data);

        // Read before crash - should see real data (phantom wasn't persisted)
        let mut buf = vec![0u8; SECTOR_SIZE];
        storage.read(0, &mut buf).expect("read failed");
        assert_eq!(buf, real_data);

        // Apply crash - phantom write just disappears
        storage.apply_crash(1.0);

        // Read after crash - should still see real data
        storage.read(0, &mut buf).expect("read failed");
        assert_eq!(buf, real_data);
    }

    #[test]
    fn test_crash_faults_pending_writes() {
        let mut storage = InMemoryStorage::new(4096, 42);

        // Write data without sync
        let data = vec![0xAA; SECTOR_SIZE];
        storage.write(0, &data, false).expect("write failed");

        // Apply crash with 100% fault probability
        storage.apply_crash(1.0);

        // The sector should now be faulted
        assert!(storage.has_fault(0));

        // Reading should return corrupted data
        let mut buf = vec![0u8; SECTOR_SIZE];
        storage.read(0, &mut buf).expect("read failed");
        assert_ne!(buf, data);
    }

    #[test]
    fn test_sync_clears_pending() {
        let mut storage = InMemoryStorage::new(4096, 42);

        // Write data without sync
        let data = vec![0xAA; SECTOR_SIZE];
        storage.write(0, &data, false).expect("write failed");

        // Sync
        storage.sync();

        // Apply crash - should not affect synced write
        storage.apply_crash(1.0);

        // Sector should not be faulted
        assert!(!storage.has_fault(0));

        // Data should be intact
        let mut buf = vec![0u8; SECTOR_SIZE];
        storage.read(0, &mut buf).expect("read failed");
        assert_eq!(buf, data);
    }

    #[test]
    fn test_sector_bitset() {
        let mut bitset = SectorBitSet::new(100);

        assert!(!bitset.is_set(0));
        assert!(!bitset.is_set(50));
        assert!(!bitset.is_set(99));

        bitset.set(0);
        bitset.set(50);
        bitset.set(99);

        assert!(bitset.is_set(0));
        assert!(bitset.is_set(50));
        assert!(bitset.is_set(99));
        assert!(!bitset.is_set(1));

        bitset.clear(50);
        assert!(!bitset.is_set(50));

        assert_eq!(bitset.len(), 100);
    }

    #[test]
    fn test_read_past_end() {
        let storage = InMemoryStorage::new(1024, 42);

        let mut buf = vec![0u8; 100];
        let result = storage.read(1000, &mut buf);

        assert!(result.is_err());
    }

    #[test]
    fn test_write_past_end() {
        let mut storage = InMemoryStorage::new(1024, 42);

        let data = vec![0u8; 100];
        let result = storage.write(1000, &data, true);

        assert!(result.is_err());
    }

    #[test]
    fn test_read_misdirected() {
        let mut storage = InMemoryStorage::new(4096, 42);

        // Write distinct data at different locations
        let data0 = vec![0x11; SECTOR_SIZE];
        let data1 = vec![0x22; SECTOR_SIZE];
        let data2 = vec![0x33; SECTOR_SIZE];

        storage.write(0, &data0, true).expect("write failed");
        storage
            .write(SECTOR_SIZE as u64, &data1, true)
            .expect("write failed");
        storage
            .write(2 * SECTOR_SIZE as u64, &data2, true)
            .expect("write failed");

        // Misdirected read from offset 0 should return data from somewhere else
        let mut buf = vec![0u8; SECTOR_SIZE];
        storage.read_misdirected(0, &mut buf).expect("read failed");

        // Should NOT be the data at offset 0
        assert_ne!(buf, data0);
    }

    #[test]
    fn test_partial_sector_read() {
        let mut storage = InMemoryStorage::new(4096, 42);

        // Write a full sector with repeating pattern
        let data: Vec<u8> = (0..SECTOR_SIZE).map(|i| (i % 256) as u8).collect();
        storage.write(0, &data, true).expect("write failed");

        // Read partial sector
        let mut buf = vec![0u8; 100];
        storage.read(50, &mut buf).expect("read failed");

        assert_eq!(buf, &data[50..150]);
    }

    #[test]
    fn test_multi_sector_read() {
        let mut storage = InMemoryStorage::new(4096, 42);

        // Write across multiple sectors
        let data = vec![0xAB; SECTOR_SIZE * 3];
        storage.write(0, &data, true).expect("write failed");

        // Read it all back
        let mut buf = vec![0u8; SECTOR_SIZE * 3];
        storage.read(0, &mut buf).expect("read failed");

        assert_eq!(buf, data);
    }
}
