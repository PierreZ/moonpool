//! POSIX shared memory allocation for cross-process data sharing.
//!
//! Provides `mmap(MAP_SHARED | MAP_ANONYMOUS)` wrappers for allocating memory
//! visible across `fork()` boundaries. This is the foundation for all
//! cross-process state in the explorer.

use std::io;

/// Allocate a shared-memory array, rejecting byte-size overflow before
/// calling `mmap`.
pub(crate) fn alloc_shared_array(count: usize, element_size: usize) -> Result<*mut u8, io::Error> {
    let size = count.checked_mul(element_size).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "shared memory size overflow")
    })?;
    alloc_shared(size)
}

/// Allocate two shared-memory regions without leaking the first if the
/// second allocation fails.
pub(crate) fn alloc_shared_pair(
    first_size: usize,
    second_size: usize,
) -> Result<(*mut u8, *mut u8), io::Error> {
    let first = alloc_shared(first_size)?;
    match alloc_shared(second_size) {
        Ok(second) => Ok((first, second)),
        Err(error) => {
            // Safety: `first` was just returned by `alloc_shared(first_size)`
            // and ownership has not escaped this function.
            unsafe { free_shared(first, first_size) };
            Err(error)
        }
    }
}

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
    // Safety: FFI call to libc::mmap.
    // - MAP_ANONYMOUS: no file descriptor required (fd = -1).
    // - MAP_SHARED: memory visible across fork() boundaries.
    // - Kernel guarantees: returned memory is zeroed and page-aligned.
    // - We check for MAP_FAILED before returning the pointer.
    // - The caller owns the returned pointer and must free it via free_shared().
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
    Ok(ptr.cast::<u8>())
}

/// Free shared memory allocated by [`alloc_shared`].
///
/// # Safety
///
/// - `ptr` must have been returned by [`alloc_shared`] with the same `size`.
/// - `ptr` must not have been previously freed (no double-free).
/// - The pointer and any derived references become invalid after this call.
/// - `size` must exactly match the value passed to `alloc_shared`.
#[cfg(unix)]
pub unsafe fn free_shared(ptr: *mut u8, size: usize) {
    unsafe {
        libc::munmap(ptr.cast::<libc::c_void>(), size);
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

    #[test]
    fn array_size_overflow_is_rejected() {
        let error = alloc_shared_array(usize::MAX, 2).expect_err("size must overflow");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn pair_allocates_independent_regions() {
        let (first, second) = alloc_shared_pair(16, 32).expect("allocate pair");
        assert_ne!(first, second);

        // Safety: both pointers were returned by alloc_shared_pair with the
        // corresponding sizes and have not been freed yet.
        unsafe {
            free_shared(first, 16);
            free_shared(second, 32);
        }
    }
}
