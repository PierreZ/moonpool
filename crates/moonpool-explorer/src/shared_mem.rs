//! Owned POSIX shared memory for cross-process state.
//!
//! [`SharedMemory`] keeps the pointer and allocation length together, so an
//! error path cannot leak an earlier mapping and callers cannot unmap with the
//! wrong length. The mapping remains visible across `fork()` as before.

use std::io;
use std::ptr::NonNull;

/// An owned, zero-initialized `MAP_SHARED | MAP_ANONYMOUS` mapping.
#[derive(Debug)]
pub(crate) struct SharedMemory {
    ptr: NonNull<u8>,
    len: usize,
}

impl SharedMemory {
    /// Allocate `len` bytes of shared memory.
    pub(crate) fn new(len: usize) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared memory length must be non-zero",
            ));
        }

        #[cfg(unix)]
        {
            // Safety: anonymous mmap ignores the fd and offset. The returned
            // pointer is checked against MAP_FAILED before becoming owned.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            let Some(ptr) = NonNull::new(ptr.cast::<u8>()) else {
                // A zero-address mapping is valid to POSIX but cannot be used
                // by this crate because null is its uninitialized sentinel.
                // Safety: mmap just returned this mapping for `len` bytes.
                unsafe { libc::munmap(ptr.cast::<libc::c_void>(), len) };
                return Err(io::Error::other(
                    "mmap returned a null address reserved as a sentinel",
                ));
            };
            Ok(Self { ptr, len })
        }

        #[cfg(not(unix))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "shared memory requires unix",
        ))
    }

    /// Allocate a checked `count * element_size` byte mapping.
    pub(crate) fn array(count: usize, element_size: usize) -> io::Result<Self> {
        let len = count.checked_mul(element_size).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "shared memory size overflow")
        })?;
        Self::new(len)
    }

    /// Return the start of the mapping.
    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // Safety: `ptr` was returned by mmap for exactly `len` bytes and
            // this owner calls munmap once, during drop.
            unsafe {
                libc::munmap(self.ptr.as_ptr().cast::<libc::c_void>(), self.len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_is_zeroed_and_writable() {
        let memory = SharedMemory::new(4096).expect("allocate mapping");
        // Safety: both offsets are within this live 4096-byte mapping.
        unsafe {
            assert_eq!(*memory.as_ptr(), 0);
            assert_eq!(*memory.as_ptr().add(4095), 0);
            *memory.as_ptr() = 42;
            *memory.as_ptr().add(4095) = 99;
            assert_eq!(*memory.as_ptr(), 42);
            assert_eq!(*memory.as_ptr().add(4095), 99);
        }
    }

    #[test]
    fn array_rejects_invalid_sizes() {
        let overflow = SharedMemory::array(usize::MAX, 2).expect_err("size must overflow");
        assert_eq!(overflow.kind(), io::ErrorKind::InvalidInput);

        let empty = SharedMemory::array(0, 16).expect_err("empty mappings are invalid");
        assert_eq!(empty.kind(), io::ErrorKind::InvalidInput);
    }
}
