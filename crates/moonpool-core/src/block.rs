//! [`BlockFile`]: a thin block-addressed view of one already-open file.
//!
//! ```text
//! database / journal / pager
//!             │
//!             ▼
//!       BlockFile<F>
//!             │
//!             ▼
//!        StorageFile
//!         ╱        ╲
//!    simulated    real
//!   file backend  file backend
//! ```
//!
//! Storage engines address their data in fixed-size blocks: page 7, block 512.
//! Translating that to byte offsets, and turning the partial transfers a file
//! is allowed to return into the whole-block transfers an engine needs, is
//! bookkeeping every such engine would otherwise repeat. That bookkeeping —
//! and nothing else — is what this type is.
//!
//! # What it is not
//!
//! `BlockFile` wraps exactly **one already-open file**, and it never touches
//! the filesystem namespace. It takes no path, stores no path, and cannot
//! open, create, rename, or delete anything: the caller opens the file and
//! hands it over.
//!
//! ```ignore
//! let file = provider.open(path, options).await?;
//! let blocks = BlockFile::new(file, block_size)?;
//! ```
//!
//! There is deliberately no `BlockFile::open`, no `BlockProvider`, and no
//! block-level equivalent of `sync_dir`: an operation that needs a path is an
//! operation for [`StorageProvider`](crate::StorageProvider). Nor does a
//! `BlockFile` represent a directory, several files, a region table, a
//! manifest, a namespace, or a virtual disk. It is one file, counted in
//! blocks.
//!
//! # What a block is not
//!
//! The block size is the *caller's* unit and nothing else:
//!
//! - it is **not** the file's I/O alignment ([`IoConstraints`]), though it
//!   must be a multiple of it — a 16 KiB page on a device that transfers in
//!   512-byte units is perfectly ordinary;
//! - it is **not** a crash-atomicity unit. Nothing here makes a block-sized
//!   write atomic, and the simulator will happily tear one;
//! - it is **not** durability. `write_blocks` returns when the bytes are
//!   visible; only [`sync`](BlockFile::sync) makes them durable.

use std::io;

use crate::{AlignedBuf, IoConstraints, StorageFile};

/// A block-addressed view of one open file.
///
/// `F` is the [`StorageFile`] this borrows its bytes, durability, and
/// alignment from; `BlockFile` adds block arithmetic and whole-transfer loops
/// on top and holds no state of its own beyond the block size.
///
/// See the [module docs](self) for what this deliberately is not.
#[derive(Debug)]
pub struct BlockFile<F> {
    file: F,
    block_size: usize,
    /// The granularity at which a transfer may be resumed: the coarsest of the
    /// file's three alignments, and `1` on an unconstrained file. See
    /// [`BlockFile::read_blocks`] for why resuming anywhere else is invalid.
    transfer_step: usize,
}

impl<F: StorageFile> BlockFile<F> {
    /// Wrap an already-open file, addressing it in `block_size`-byte blocks.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if `block_size` is zero, or if
    /// it does not satisfy the file's I/O constraints — a block that cannot be
    /// transferred in one call is not a block size the file can honour. On a
    /// buffered file any positive size is accepted.
    pub fn new(file: F, block_size: usize) -> io::Result<Self> {
        if block_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "block size must be greater than zero",
            ));
        }
        let constraints = file.constraints();
        if !block_size.is_multiple_of(constraints.length_alignment()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "block size {block_size} is not a multiple of the file's length alignment {}",
                    constraints.length_alignment()
                ),
            ));
        }
        let offset_alignment = constraints.offset_alignment();
        if !(block_size as u64).is_multiple_of(offset_alignment) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "block size {block_size} is not a multiple of the file's offset alignment \
                     {offset_alignment}, so block boundaries would be unaddressable"
                ),
            ));
        }
        // All three alignments are powers of two, so the coarsest of them is
        // their least common multiple: a boundary that satisfies all three at
        // once. `block_size` is a multiple of each (checked above), so it is a
        // multiple of the step, and every request the transfer loops issue
        // starts on a step boundary inside a block-aligned range.
        let offset_alignment = usize::try_from(constraints.offset_alignment()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the file's offset alignment does not fit in memory",
            )
        })?;
        let transfer_step = offset_alignment
            .max(constraints.length_alignment())
            .max(constraints.memory_alignment());
        Ok(Self {
            file,
            block_size,
            transfer_step,
        })
    }

    /// The block size this view was built with.
    #[must_use]
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// The alignment the underlying file requires, unchanged.
    ///
    /// Reported so a caller can allocate buffers this file will accept without
    /// reaching past the block layer; see [`buffer`](Self::buffer).
    #[must_use]
    pub fn constraints(&self) -> IoConstraints {
        self.file.constraints()
    }

    /// The file underneath, for the operations that are not block-shaped.
    #[must_use]
    pub fn get_ref(&self) -> &F {
        &self.file
    }

    /// Unwrap the file, discarding the block view.
    #[must_use]
    pub fn into_inner(self) -> F {
        self.file
    }

    /// Allocate a zeroed buffer of `blocks` blocks that satisfies the file's
    /// memory alignment.
    ///
    /// Reuses [`AlignedBuf`] rather than growing a second aligned allocator:
    /// direct I/O needs an aligned buffer, and this is where a block-shaped
    /// caller gets one.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] if `blocks` blocks do not fit in
    /// memory. Returning that beats wrapping to a small allocation, which
    /// would hand back a buffer of the wrong size in release builds.
    pub fn buffer(&self, blocks: usize) -> io::Result<AlignedBuf> {
        let bytes = blocks.checked_mul(self.block_size).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{blocks} blocks of {} bytes do not fit in memory",
                    self.block_size
                ),
            )
        })?;
        Ok(AlignedBuf::for_constraints(bytes, self.constraints()))
    }

    /// Read whole blocks starting at `block_index` into `buf`.
    ///
    /// `buf.len()` must be a non-zero multiple of the block size, and `buf`
    /// must satisfy the file's memory alignment — [`buffer`](Self::buffer)
    /// returns one that does. Unlike [`StorageFile::read_at`], this fills the
    /// buffer completely or fails: short reads are looped over, and a range
    /// that runs past the end of the file is
    /// [`io::ErrorKind::UnexpectedEof`]. Handling partial transfers is exactly
    /// the job this layer exists to do.
    ///
    /// # Resuming a short read
    ///
    /// Every request this issues is valid for the file's constraints, however
    /// the previous one stopped. A short read may end anywhere the file's
    /// contract allows — not necessarily on an alignment boundary — so
    /// resuming at `offset + moved` could ask for a misaligned offset, a
    /// misaligned length, or a misaligned buffer address, all of which a
    /// direct-I/O file is entitled to refuse. Instead the next request starts
    /// at the alignment boundary at or below the frontier and re-reads the few
    /// bytes in between. On an unconstrained file that boundary *is* the
    /// frontier, so nothing is ever read twice.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] for a buffer that is not a whole number
    /// of blocks or does not meet the file's memory alignment,
    /// [`io::ErrorKind::UnexpectedEof`] if the file ends inside the range,
    /// [`io::ErrorKind::InvalidData`] if the file's short transfers are finer
    /// than its own alignment allows a caller to resume from, and any error
    /// the file itself reports.
    pub async fn read_blocks(&self, block_index: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = self.range(block_index, buf.len())?;
        self.check_buffer(buf)?;
        let len = buf.len();
        let mut done = 0;
        while done < len {
            // Resume from the step boundary at or below the frontier, not from
            // the frontier itself. A short transfer may stop anywhere the
            // file's own contract allows, and continuing from there would ask
            // for a misaligned offset, a misaligned length, or a misaligned
            // buffer address — a request the file is entitled to refuse. The
            // bytes between the boundary and the frontier are simply read
            // again.
            let from = self.align_down(done);
            let moved = self
                .file
                .read_at(start + from as u64, &mut buf[from..])
                .await?;
            if moved == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "file ended {done} bytes into a {len}-byte read at block {block_index}"
                    ),
                ));
            }
            done = self.advance(done, from, moved, "read")?;
        }
        Ok(())
    }

    /// Write whole blocks starting at `block_index`.
    ///
    /// `buf.len()` must be a non-zero multiple of the block size, and `buf`
    /// must satisfy the file's memory alignment. Short writes are looped over
    /// the same way [`read_blocks`](Self::read_blocks) loops over short reads
    /// — resuming on an alignment boundary and rewriting the bytes in between,
    /// which is safe because they are the same bytes from the same buffer — so
    /// on `Ok` every byte has been written. Writing past the end of the file
    /// extends it.
    ///
    /// Completion means the bytes are *visible*, not durable: only a following
    /// [`sync`](Self::sync) makes them survive a crash, whether or not the
    /// file was opened for direct I/O.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] for a buffer that is not a whole number
    /// of blocks or does not meet the file's memory alignment,
    /// [`io::ErrorKind::WriteZero`] if the file stops accepting bytes,
    /// [`io::ErrorKind::InvalidData`] if the file's short transfers are finer
    /// than its own alignment allows a caller to resume from, and any error
    /// the file itself reports.
    pub async fn write_blocks(&self, block_index: u64, buf: &[u8]) -> io::Result<()> {
        let start = self.range(block_index, buf.len())?;
        self.check_buffer(buf)?;
        let len = buf.len();
        let mut done = 0;
        while done < len {
            // As in `read_blocks`: resume on a step boundary, rewriting the
            // bytes between it and the frontier. Rewriting them is safe — they
            // are the same bytes from the same buffer.
            let from = self.align_down(done);
            let moved = self
                .file
                .write_at(start + from as u64, &buf[from..])
                .await?;
            if moved == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("file accepted only {done} of {len} bytes at block {block_index}"),
                ));
            }
            done = self.advance(done, from, moved, "write")?;
        }
        Ok(())
    }

    /// Make every completed write durable, data and length alike.
    ///
    /// # Errors
    ///
    /// Any error the file's sync reports.
    pub async fn sync(&self) -> io::Result<()> {
        self.file.sync_all().await
    }

    /// Make completed writes durable without waiting for metadata.
    ///
    /// # Errors
    ///
    /// Any error the file's sync reports.
    pub async fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data().await
    }

    /// The file's size in whole blocks, rounding a partial trailing block
    /// down: a block that is not all there is not a block this view can read.
    ///
    /// # Errors
    ///
    /// Any error the file's size query reports.
    pub async fn size_in_blocks(&self) -> io::Result<u64> {
        Ok(self.file.size().await? / self.block_size as u64)
    }

    /// Grow the file to at least `blocks` blocks, leaving a larger file alone.
    ///
    /// This makes the range addressable; it does not allocate physical
    /// storage, initialize the new bytes, or make the new length durable —
    /// three separate things, none of which this is. The new blocks read
    /// whatever the file's backend leaves there, which the simulator makes a
    /// point of varying.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] if the requested size overflows, and
    /// any error the file's size query or resize reports.
    pub async fn grow_to_blocks(&self, blocks: u64) -> io::Result<()> {
        let wanted = blocks.checked_mul(self.block_size as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "file size overflows u64")
        })?;
        if self.file.size().await? >= wanted {
            return Ok(());
        }
        self.file.set_len(wanted).await
    }

    /// Round a byte count down to a boundary every one of the file's three
    /// alignments accepts. The identity on an unconstrained file.
    fn align_down(&self, offset: usize) -> usize {
        offset & !(self.transfer_step - 1)
    }

    /// Fold one completed transfer into the frontier.
    ///
    /// # Errors
    ///
    /// A transfer that delivers nothing past the frontier cannot be continued:
    /// the next request would start at the same boundary and ask for the same
    /// bytes, so looping would spin forever. That is a file whose short
    /// transfers are finer than its own alignment allows a caller to resume
    /// from, which is a contradiction in its contract rather than a condition
    /// to retry.
    fn advance(&self, done: usize, from: usize, moved: usize, verb: &str) -> io::Result<usize> {
        let reached = from + moved;
        if reached <= done {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "a {moved}-byte short {verb} from offset {from} left the transfer at {done} \
                     bytes, which the file's {}-byte alignment cannot resume past",
                    self.transfer_step
                ),
            ));
        }
        Ok(reached)
    }

    /// Check the caller's buffer against the file's constraints, before the
    /// first request rather than inside it.
    ///
    /// The length is already known to be a whole number of blocks, and a block
    /// is a multiple of the length alignment; what remains is the buffer's
    /// address. [`BlockFile::buffer`] returns one that satisfies it.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] naming the alignment the buffer misses.
    fn check_buffer(&self, buf: &[u8]) -> io::Result<()> {
        let memory_alignment = self.constraints().memory_alignment();
        if (buf.as_ptr() as usize).is_multiple_of(memory_alignment) {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "this file requires {memory_alignment}-byte aligned buffers; \
                 use BlockFile::buffer to allocate one"
            ),
        ))
    }

    /// Byte offset of a block-aligned transfer, validating its length and
    /// that the whole range is addressable.
    ///
    /// Both ends are checked: a block index can overflow a byte offset on its
    /// own, and an index that fits can still name a range whose *end* does
    /// not. Neither may wrap — a wrapped offset addresses the wrong part of
    /// the file rather than failing.
    fn range(&self, block_index: u64, len: usize) -> io::Result<u64> {
        if len == 0 || !len.is_multiple_of(self.block_size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "transfer of {len} bytes is not a non-zero multiple of the {}-byte block size",
                    self.block_size
                ),
            ));
        }
        let start = block_index
            .checked_mul(self.block_size as u64)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("block index {block_index} overflows a byte offset"),
                )
            })?;
        start.checked_add(len as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a {len}-byte transfer at block {block_index} runs past the end of the address space"),
            )
        })?;
        Ok(start)
    }
}
