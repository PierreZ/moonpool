//! Coverage tracking for exploration novelty detection.
//!
//! Tracks which assertion paths have been explored across all timelines.
//! The [`ExploredMap`] lives in shared memory and accumulates coverage
//! from every timeline. Each timeline gets a fresh [`CoverageBitmap`]
//! that is merged into the explored map after the timeline finishes.

/// Size of coverage bitmaps in bytes (8192 bit positions).
pub const COVERAGE_MAP_SIZE: usize = 1024;

/// Per-timeline coverage bitmap, cleared before each split.
///
/// Tracks which assertion paths were hit during this timeline's execution.
/// After the timeline finishes, the parent merges this into the [`ExploredMap`].
pub struct CoverageBitmap {
    ptr: *mut u8,
}

impl CoverageBitmap {
    /// Wrap a shared-memory pointer as a coverage bitmap.
    ///
    /// # Safety
    ///
    /// `ptr` must point to at least [`COVERAGE_MAP_SIZE`] bytes of valid,
    /// writable shared memory.
    pub unsafe fn new(ptr: *mut u8) -> Self {
        Self { ptr }
    }

    /// Set the bit at the given index (mod total bits).
    pub fn set_bit(&self, index: usize) {
        let bit_index = index % (COVERAGE_MAP_SIZE * 8);
        let byte = bit_index / 8;
        let bit = bit_index % 8;
        // Safety: ptr points to COVERAGE_MAP_SIZE bytes, byte < COVERAGE_MAP_SIZE
        unsafe {
            *self.ptr.add(byte) |= 1 << bit;
        }
    }

    /// Clear all bits to zero.
    pub fn clear(&self) {
        // Safety: ptr points to COVERAGE_MAP_SIZE bytes
        unsafe {
            std::ptr::write_bytes(self.ptr, 0, COVERAGE_MAP_SIZE);
        }
    }

    /// Get a pointer to the underlying data.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }
}

/// Cross-process coverage map, OR'd by all timelines.
///
/// Lives in `MAP_SHARED` memory so all forked timelines contribute.
/// A bit set to 1 means "this assertion path has been explored."
pub struct ExploredMap {
    ptr: *mut u8,
}

impl ExploredMap {
    /// Wrap a shared-memory pointer as a explored map.
    ///
    /// # Safety
    ///
    /// `ptr` must point to at least [`COVERAGE_MAP_SIZE`] bytes of valid,
    /// writable shared memory.
    pub unsafe fn new(ptr: *mut u8) -> Self {
        Self { ptr }
    }

    /// Merge a timeline's coverage bitmap into this explored map (bitwise OR).
    pub fn merge_from(&self, other: &CoverageBitmap) {
        // Safety: both pointers are valid for COVERAGE_MAP_SIZE bytes
        unsafe {
            for i in 0..COVERAGE_MAP_SIZE {
                *self.ptr.add(i) |= *other.as_ptr().add(i);
            }
        }
    }

    /// Count the number of set bits in the explored map (population count).
    ///
    /// Returns the total number of unique assertion paths explored across
    /// all timelines.
    pub fn count_bits_set(&self) -> u32 {
        let mut count: u32 = 0;
        // Safety: ptr points to COVERAGE_MAP_SIZE bytes
        unsafe {
            for i in 0..COVERAGE_MAP_SIZE {
                count += (*self.ptr.add(i)).count_ones();
            }
        }
        count
    }

    /// Check if a timeline's bitmap contains any bits not yet in the explored map.
    pub fn has_new_bits(&self, other: &CoverageBitmap) -> bool {
        // Safety: both pointers are valid for COVERAGE_MAP_SIZE bytes
        unsafe {
            for i in 0..COVERAGE_MAP_SIZE {
                let explored = *self.ptr.add(i);
                let child = *other.as_ptr().add(i);
                // Child has bits that explored map doesn't
                if (child & !explored) != 0 {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_mem;

    #[test]
    fn test_set_bit_and_check() {
        let ptr = shared_mem::alloc_shared(COVERAGE_MAP_SIZE).expect("alloc failed");
        let explored_ptr = shared_mem::alloc_shared(COVERAGE_MAP_SIZE).expect("alloc failed");
        let bm = unsafe { CoverageBitmap::new(ptr) };
        let vm = unsafe { ExploredMap::new(explored_ptr) };

        // Initially no new bits
        assert!(!vm.has_new_bits(&bm));

        // Set a bit in bitmap
        bm.set_bit(42);
        assert!(vm.has_new_bits(&bm));

        // Merge into explored map
        vm.merge_from(&bm);
        // Now no new bits (already merged)
        assert!(!vm.has_new_bits(&bm));

        unsafe {
            shared_mem::free_shared(ptr, COVERAGE_MAP_SIZE);
            shared_mem::free_shared(explored_ptr, COVERAGE_MAP_SIZE);
        }
    }

    #[test]
    fn test_clear() {
        let ptr = shared_mem::alloc_shared(COVERAGE_MAP_SIZE).expect("alloc failed");
        let bm = unsafe { CoverageBitmap::new(ptr) };

        bm.set_bit(0);
        bm.set_bit(100);
        bm.set_bit(8000);

        bm.clear();

        // Verify all zeroed
        unsafe {
            for i in 0..COVERAGE_MAP_SIZE {
                assert_eq!(*ptr.add(i), 0);
            }
            shared_mem::free_shared(ptr, COVERAGE_MAP_SIZE);
        }
    }

    #[test]
    fn test_merge_accumulates() {
        let bm1_ptr = shared_mem::alloc_shared(COVERAGE_MAP_SIZE).expect("alloc failed");
        let bm2_ptr = shared_mem::alloc_shared(COVERAGE_MAP_SIZE).expect("alloc failed");
        let vm_ptr = shared_mem::alloc_shared(COVERAGE_MAP_SIZE).expect("alloc failed");

        let bm1 = unsafe { CoverageBitmap::new(bm1_ptr) };
        let bm2 = unsafe { CoverageBitmap::new(bm2_ptr) };
        let vm = unsafe { ExploredMap::new(vm_ptr) };

        bm1.set_bit(10);
        bm2.set_bit(20);

        vm.merge_from(&bm1);
        // bm2 has bit 20 which is new
        assert!(vm.has_new_bits(&bm2));

        vm.merge_from(&bm2);
        // Now neither has new bits
        assert!(!vm.has_new_bits(&bm1));
        assert!(!vm.has_new_bits(&bm2));

        unsafe {
            shared_mem::free_shared(bm1_ptr, COVERAGE_MAP_SIZE);
            shared_mem::free_shared(bm2_ptr, COVERAGE_MAP_SIZE);
            shared_mem::free_shared(vm_ptr, COVERAGE_MAP_SIZE);
        }
    }

    #[test]
    fn test_count_bits_set() {
        let vm_ptr = shared_mem::alloc_shared(COVERAGE_MAP_SIZE).expect("alloc failed");
        let bm_ptr = shared_mem::alloc_shared(COVERAGE_MAP_SIZE).expect("alloc failed");
        let vm = unsafe { ExploredMap::new(vm_ptr) };
        let bm = unsafe { CoverageBitmap::new(bm_ptr) };

        assert_eq!(vm.count_bits_set(), 0);

        bm.set_bit(0);
        bm.set_bit(42);
        bm.set_bit(8000);
        vm.merge_from(&bm);

        assert_eq!(vm.count_bits_set(), 3);

        unsafe {
            shared_mem::free_shared(vm_ptr, COVERAGE_MAP_SIZE);
            shared_mem::free_shared(bm_ptr, COVERAGE_MAP_SIZE);
        }
    }
}
