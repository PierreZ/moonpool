//! One durable positioned write as one compio operation:
//! `pwritev2(RWF_DSYNC)`, the operation the Tokio backend issues.
//!
//! compio has no such operation, so [`WriteAtDsync`] is a custom one for the
//! fusion driver. On `io_uring` it is an `IORING_OP_WRITE` entry carrying
//! `RWF_DSYNC` in its `rw_flags`; on the polling driver (and on an `io_uring`
//! kernel without `IORING_OP_WRITE`) it is a `pwritev2` call on compio's
//! blocking pool. Either way the bytes it reports written are durable when it
//! completes, in one operation where a write plus `sync_data` takes two.

use std::io;
use std::os::fd::{AsFd, AsRawFd as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

use compio::BufResult;
use compio::buf::{IntoInner, IoBuf};
use compio::driver::{Decision, IourOpCode, OpEntry, PollOpCode, ToSharedFd as _};
use compio::fs::File;

/// Set once the kernel refused `RWF_DSYNC`: every later durable write in the
/// process goes straight to the write-then-`sync_data` fallback instead of
/// failing the same operation again (the Tokio backend's policy).
static DSYNC_REFUSED: AtomicBool = AtomicBool::new(false);

/// Whether a durable write's failure means *this kernel cannot do
/// `RWF_DSYNC`*: no `pwritev2` (`ENOSYS`) or no `RWF_DSYNC` (`EOPNOTSUPP`).
/// The same two codes as the Tokio backend; every other error is a real
/// failure of the write.
fn is_dsync_unsupported(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EOPNOTSUPP))
}

/// Write `buffer` at `offset` durably: one `RWF_DSYNC` write, or, once the
/// kernel refused one, a write then `sync_data`. The buffer comes back with
/// the result, whichever path ran.
pub(super) async fn write_at_durable<T: IoBuf>(
    file: &File,
    offset: u64,
    buffer: T,
) -> BufResult<usize, T> {
    if DSYNC_REFUSED.load(Ordering::Relaxed) {
        return super::write_then_sync(file, offset, buffer).await;
    }
    let op = WriteAtDsync::new(file.to_shared_fd(), offset, buffer);
    let BufResult(result, buffer) = compio::runtime::submit(op).await.into_inner();
    match result {
        Err(error) if is_dsync_unsupported(&error) => {
            DSYNC_REFUSED.store(true, Ordering::Relaxed);
            tracing::debug!(
                %error,
                "RWF_DSYNC refused, durable writes fall back to write then sync_data"
            );
            super::write_then_sync(file, offset, buffer).await
        }
        result => {
            tracing::debug!(offset, "durable write via RWF_DSYNC");
            BufResult(result, buffer)
        }
    }
}

/// A positioned write that carries its own data sync: `pwritev2` with
/// `RWF_DSYNC`. Like `O_DSYNC`, it covers this write's range only.
///
/// The operation owns its buffer and a shared handle to the descriptor, as
/// compio's own `WriteAt` does: the driver may keep the operation alive past
/// a dropped future (cancellation), so neither may be borrowed.
pub(super) struct WriteAtDsync<T: IoBuf, S> {
    fd: S,
    offset: u64,
    buffer: T,
}

impl<T: IoBuf, S> WriteAtDsync<T, S> {
    /// Write `buffer` at `offset` of `fd`, durably.
    pub(super) fn new(fd: S, offset: u64, buffer: T) -> Self {
        Self { fd, offset, buffer }
    }
}

impl<T: IoBuf, S: AsFd> WriteAtDsync<T, S> {
    /// The blocking form: one `pwritev2(RWF_DSYNC)` syscall, retried when a
    /// signal interrupts it.
    fn pwritev2(&self) -> io::Result<usize> {
        let slice = self.buffer.as_init();
        let position = libc::off_t::try_from(self.offset).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("offset {} overflows off_t", self.offset),
            )
        })?;
        let iov = libc::iovec {
            iov_base: slice.as_ptr().cast_mut().cast::<libc::c_void>(),
            iov_len: slice.len(),
        };
        loop {
            // SAFETY: the descriptor is open for the duration of the call
            // (the operation owns a handle to it), `iov` describes exactly
            // the operation's buffer, which it owns and keeps unmoved past
            // the call, and the kernel only reads through it.
            let written = unsafe {
                libc::pwritev2(
                    self.fd.as_fd().as_raw_fd(),
                    &raw const iov,
                    1,
                    position,
                    libc::RWF_DSYNC,
                )
            };
            if let Ok(written) = usize::try_from(written) {
                return Ok(written);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl<T: IoBuf, S> IntoInner for WriteAtDsync<T, S> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

// SAFETY: the entry points at the operation's own buffer, which the operation
// owns and the driver keeps unmoved (heap-allocated with the operation) until
// the operation completes; the descriptor is a shared handle the operation
// owns for the same span.
unsafe impl<T: IoBuf, S: AsFd> IourOpCode for WriteAtDsync<T, S> {
    type Control = ();

    fn create_entry(&mut self, (): &mut Self::Control) -> OpEntry {
        let slice = self.buffer.as_init();
        io_uring::opcode::Write::new(
            io_uring::types::Fd(self.fd.as_fd().as_raw_fd()),
            slice.as_ptr(),
            u32::try_from(slice.len()).unwrap_or(u32::MAX),
        )
        .offset(self.offset)
        .rw_flags(libc::RWF_DSYNC)
        .build()
        .into()
    }

    /// A kernel whose `io_uring` lacks `IORING_OP_WRITE` (before 5.6) runs
    /// the operation on the blocking pool instead.
    fn call_blocking(&mut self, (): &mut Self::Control) -> io::Result<usize> {
        self.pwritev2()
    }
}

// SAFETY: `pre_submit` never returns `Decision::Wait`, so no `op_type` is
// owed; the blocking `operate` touches only the operation's own buffer and
// descriptor handle.
unsafe impl<T: IoBuf, S: AsFd> PollOpCode for WriteAtDsync<T, S> {
    type Control = ();

    /// A file write never waits for readiness: run it on the blocking pool,
    /// as compio's own `WriteAt` does on Linux.
    fn pre_submit(&mut self, (): &mut Self::Control) -> io::Result<Decision> {
        Ok(Decision::Blocking)
    }

    fn operate(&mut self, (): &mut Self::Control) -> Poll<io::Result<usize>> {
        Poll::Ready(self.pwritev2())
    }
}

#[cfg(test)]
mod tests {
    use super::{DSYNC_REFUSED, is_dsync_unsupported, write_at_durable};
    use compio::BufResult;
    use std::io;
    use std::sync::atomic::Ordering;

    /// Only "no `pwritev2`" and "no `RWF_DSYNC`" switch the process to the
    /// fallback (as in the Tokio backend); a real write failure must surface.
    #[test]
    fn only_unsupported_errors_switch_to_the_fallback() {
        assert!(is_dsync_unsupported(&io::Error::from_raw_os_error(
            libc::ENOSYS
        )));
        assert!(is_dsync_unsupported(&io::Error::from_raw_os_error(
            libc::EOPNOTSUPP
        )));
        for code in [libc::EINVAL, libc::EIO, libc::ENOSPC, libc::EBADF] {
            assert!(!is_dsync_unsupported(&io::Error::from_raw_os_error(code)));
        }
        assert!(!is_dsync_unsupported(&io::Error::other("synthetic")));
    }

    /// On the runtime's default driver (`io_uring` where the kernel allows
    /// it), a durable write is the `RWF_DSYNC` operation itself: it succeeds,
    /// returns its buffer, and does not switch the process to the fallback.
    #[compio::test]
    async fn a_durable_write_is_one_dsync_operation() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = compio::fs::File::create(dir.path().join("durable")).await?;
        let BufResult(written, buffer) = write_at_durable(&file, 3, b"durable".to_vec()).await;
        assert_eq!(written?, 7);
        assert_eq!(buffer, b"durable", "the buffer comes back");
        assert!(
            !DSYNC_REFUSED.load(Ordering::Relaxed),
            "this kernel accepts RWF_DSYNC, so no fallback ran"
        );
        assert_eq!(file.metadata().await?.len(), 10);
        Ok(())
    }
}
