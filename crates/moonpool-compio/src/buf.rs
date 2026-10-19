//! [`CompioAlignedBuf`]: moonpool's [`AlignedBuf`] as a compio owned buffer.

use std::mem::MaybeUninit;

use compio::buf::{IoBuf, IoBufMut, SetLen};
use moonpool_core::AlignedBuf;

/// An [`AlignedBuf`] handed to compio for a zero-copy positioned transfer.
///
/// compio operations take their buffer by value (the kernel owns it while
/// an `io_uring` operation is in flight) and hand it back with the result.
/// This newtype exposes **exactly** the aligned window of the buffer,
/// `[offset, offset + len)`, never the over-allocated backing: a direct-I/O
/// transfer must start on the aligned address and be exactly the aligned
/// length, so a read is sized by the window and a write sends the window.
///
/// The window is always fully initialized (an `AlignedBuf` is zeroed at
/// allocation), so the initialized length and the capacity are the same,
/// and [`SetLen::set_len`] is a no-op, as it is for `[u8]`: compio reports
/// how many bytes a read filled through the operation's result, not through
/// the buffer.
#[derive(Debug)]
pub(crate) struct CompioAlignedBuf(pub(crate) AlignedBuf);

impl IoBuf for CompioAlignedBuf {
    fn as_init(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl SetLen for CompioAlignedBuf {
    unsafe fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.0.len());
    }
}

impl IoBufMut for CompioAlignedBuf {
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        let window = self.0.as_mut_slice();
        let len = window.len();
        let ptr = window.as_mut_ptr().cast::<MaybeUninit<u8>>();
        // SAFETY: the window is fully initialized, `MaybeUninit<u8>` has the
        // layout of `u8`, and the returned slice reborrows `self` mutably for
        // its whole life. This is how compio itself implements `IoBufMut`
        // for `[u8]`.
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    }
}

#[cfg(test)]
mod tests {
    use super::CompioAlignedBuf;
    use compio::buf::{IoBuf, IoBufMut};
    use moonpool_core::AlignedBuf;

    /// The buffer compio sees starts on the aligned address and is exactly
    /// the window's length, never the backing allocation's.
    #[test]
    fn exposes_exactly_the_aligned_window() {
        let mut buf = CompioAlignedBuf(AlignedBuf::zeroed(4096, 4096));
        assert_eq!(buf.as_init().len(), 4096);
        assert!((buf.as_init().as_ptr() as usize).is_multiple_of(4096));
        let uninit = buf.as_uninit();
        assert_eq!(uninit.len(), 4096);
        assert!((uninit.as_ptr() as usize).is_multiple_of(4096));

        let mut short = AlignedBuf::zeroed(8192, 512);
        short.truncate(512);
        let short = CompioAlignedBuf(short);
        assert_eq!(short.as_init().len(), 512);
    }
}
