//! POSIX shared memory allocation for cross-process data sharing.
//!
//! Provides `mmap(MAP_SHARED | MAP_ANONYMOUS)` wrappers for allocating memory
//! visible across `fork()` boundaries. This is the foundation for all
//! cross-process state in the explorer.

use std::io;

/// Allocate shared memory visible across `fork()` boundaries.
///
/// Returns a pointer to `size` bytes of zeroed memory backed by
/// `MAP_SHARED | MAP_ANONYMOUS`. The memory is readable and writable
/// by both parent and child processes after `fork()`.
///
/// # Errors
///
/// Returns an error if `mmap` fails (e.g., insufficient memory).
#[cfg(unix)]
pub fn alloc_shared(size: usize) -> Result<*mut u8, io::Error> {
    // Safety: MAP_ANONYMOUS does not require a file descriptor.
    // MAP_SHARED ensures visibility across fork().
    // The returned memory is zeroed by the kernel.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(ptr as *mut u8)
}

/// Free shared memory allocated by [`alloc_shared`].
///
/// # Safety
///
/// `ptr` must have been returned by [`alloc_shared`] with the same `size`.
/// The pointer must not be used after this call.
#[cfg(unix)]
pub unsafe fn free_shared(ptr: *mut u8, size: usize) {
    unsafe {
        libc::munmap(ptr as *mut libc::c_void, size);
    }
}

/// No-op on non-unix platforms.
#[cfg(not(unix))]
pub fn alloc_shared(size: usize) -> Result<*mut u8, io::Error> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "shared memory requires unix",
    ))
}

/// No-op on non-unix platforms.
#[cfg(not(unix))]
pub unsafe fn free_shared(_ptr: *mut u8, _size: usize) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alloc_write_read_free() {
        let size = 4096;
        let ptr = alloc_shared(size).expect("alloc_shared failed");
        assert!(!ptr.is_null());

        // Write and read back
        unsafe {
            *ptr = 42;
            *ptr.add(size - 1) = 99;
            assert_eq!(*ptr, 42);
            assert_eq!(*ptr.add(size - 1), 99);
            free_shared(ptr, size);
        }
    }

    #[test]
    fn test_zeroed_on_alloc() {
        let size = 1024;
        let ptr = alloc_shared(size).expect("alloc_shared failed");

        // Kernel guarantees zeroed memory from mmap
        unsafe {
            for i in 0..size {
                assert_eq!(*ptr.add(i), 0);
            }
            free_shared(ptr, size);
        }
    }
}
