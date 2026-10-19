//! A stream write on an `O_APPEND` descriptor as one compio operation: a plain
//! `write(2)` with no offset.
//!
//! Linux ignores the offset of `pwrite` on an `O_APPEND` descriptor, but POSIX
//! (and macOS) honour it, so a positioned write cannot be the append. compio's
//! own `op::Write` is no substitute: on the polling driver it waits for
//! writability through epoll or kqueue, and epoll refuses regular files while
//! kqueue's write filter does not support vnodes. [`AppendWrite`] runs on the
//! blocking pool there instead, as compio's own file operations do.

use std::io;
use std::os::fd::{AsFd, AsRawFd as _};
use std::task::Poll;

use compio::BufResult;
use compio::buf::{IntoInner, IoBuf, IoBufExt as _};
use compio::driver::{Decision, PollOpCode, ToSharedFd as _};
#[cfg(target_os = "linux")]
use compio::driver::{IourOpCode, OpEntry};
use compio::fs::File;

/// Append all of `data` to the file behind `file`, which must have been opened
/// with `O_APPEND`. A `write(2)` may be short, so this loops until every byte
/// is written.
pub(super) async fn append_all(file: &File, data: Vec<u8>) -> io::Result<()> {
    let mut data = data;
    let mut written = 0;
    while written < data.len() {
        let op = AppendWrite::new(file.to_shared_fd(), data.slice(written..));
        let BufResult(result, slice) = compio::runtime::submit(op).await.into_inner();
        data = slice.into_inner();
        let round = result?;
        if round == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        written += round;
    }
    Ok(())
}

/// One `write(2)` on an `O_APPEND` descriptor: the kernel places the bytes at
/// the end of the file, atomically.
///
/// The operation owns its buffer and a shared handle to the descriptor, as
/// compio's own operations do: the driver may keep it alive past a dropped
/// future (cancellation), so neither may be borrowed.
pub(super) struct AppendWrite<T: IoBuf, S> {
    fd: S,
    buffer: T,
}

impl<T: IoBuf, S> AppendWrite<T, S> {
    /// Append `buffer` through `fd`.
    pub(super) fn new(fd: S, buffer: T) -> Self {
        Self { fd, buffer }
    }
}

impl<T: IoBuf, S: AsFd> AppendWrite<T, S> {
    /// The blocking form: one `write` syscall, retried when a signal
    /// interrupts it.
    fn write(&self) -> io::Result<usize> {
        let slice = self.buffer.as_init();
        loop {
            // SAFETY: the descriptor is open for the duration of the call
            // (the operation owns a handle to it), the pointer and length
            // describe exactly the operation's buffer, which it owns and
            // keeps unmoved past the call, and the kernel only reads through
            // it.
            let written = unsafe {
                libc::write(
                    self.fd.as_fd().as_raw_fd(),
                    slice.as_ptr().cast::<libc::c_void>(),
                    slice.len(),
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

impl<T: IoBuf, S> IntoInner for AppendWrite<T, S> {
    type Inner = T;

    fn into_inner(self) -> Self::Inner {
        self.buffer
    }
}

// SAFETY: the entry points at the operation's own buffer, which the operation
// owns and the driver keeps unmoved (heap-allocated with the operation) until
// the operation completes; the descriptor is a shared handle the operation
// owns for the same span.
#[cfg(target_os = "linux")]
unsafe impl<T: IoBuf, S: AsFd> IourOpCode for AppendWrite<T, S> {
    type Control = ();

    /// An offset of `u64::MAX` means "the file position", as in compio's own
    /// `Write`; under `O_APPEND` the kernel appends whatever it says.
    fn create_entry(&mut self, (): &mut Self::Control) -> OpEntry {
        let slice = self.buffer.as_init();
        io_uring::opcode::Write::new(
            io_uring::types::Fd(self.fd.as_fd().as_raw_fd()),
            slice.as_ptr(),
            u32::try_from(slice.len()).unwrap_or(u32::MAX),
        )
        .offset(u64::MAX)
        .build()
        .into()
    }

    /// A kernel whose `io_uring` lacks `IORING_OP_WRITE` (before 5.6) runs
    /// the operation on the blocking pool instead.
    fn call_blocking(&mut self, (): &mut Self::Control) -> io::Result<usize> {
        self.write()
    }
}

// SAFETY: `pre_submit` never returns `Decision::Wait`, so no `op_type` is
// owed; the blocking `operate` touches only the operation's own buffer and
// descriptor handle.
unsafe impl<T: IoBuf, S: AsFd> PollOpCode for AppendWrite<T, S> {
    type Control = ();

    /// A file write never waits for readiness: run it on the blocking pool.
    fn pre_submit(&mut self, (): &mut Self::Control) -> io::Result<Decision> {
        Ok(Decision::Blocking)
    }

    fn operate(&mut self, (): &mut Self::Control) -> Poll<io::Result<usize>> {
        Poll::Ready(self.write())
    }
}
