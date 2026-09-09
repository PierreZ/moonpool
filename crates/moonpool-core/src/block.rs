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
        Ok(Self { file, block_size })
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
    #[must_use]
    pub fn buffer(&self, blocks: usize) -> AlignedBuf {
        AlignedBuf::for_constraints(blocks * self.block_size, self.constraints())
    }

    /// Read whole blocks starting at `block_index` into `buf`.
    ///
    /// `buf.len()` must be a non-zero multiple of the block size. Unlike
    /// [`StorageFile::read_at`], this fills the buffer completely or fails:
    /// short reads are looped over, and a range that runs past the end of the
    /// file is [`io::ErrorKind::UnexpectedEof`]. Handling partial transfers is
    /// exactly the job this layer exists to do.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] for a buffer that is not a whole number
    /// of blocks, [`io::ErrorKind::UnexpectedEof`] if the file ends inside the
    /// range, and any error the file itself reports.
    pub async fn read_blocks(&self, block_index: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut offset = self.range(block_index, buf.len())?;
        let mut read = 0;
        while read < buf.len() {
            let moved = self.file.read_at(offset, &mut buf[read..]).await?;
            if moved == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "file ended {} bytes into a {}-byte read at block {block_index}",
                        read,
                        buf.len()
                    ),
                ));
            }
            read += moved;
            offset += moved as u64;
        }
        Ok(())
    }

    /// Write whole blocks starting at `block_index`.
    ///
    /// `buf.len()` must be a non-zero multiple of the block size. Short writes
    /// are looped over, so on `Ok` every byte has been written. Writing past
    /// the end of the file extends it.
    ///
    /// Completion means the bytes are *visible*, not durable: only a following
    /// [`sync`](Self::sync) makes them survive a crash, whether or not the
    /// file was opened for direct I/O.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] for a buffer that is not a whole number
    /// of blocks, [`io::ErrorKind::WriteZero`] if the file stops accepting
    /// bytes, and any error the file itself reports.
    pub async fn write_blocks(&self, block_index: u64, buf: &[u8]) -> io::Result<()> {
        let mut offset = self.range(block_index, buf.len())?;
        let mut written = 0;
        while written < buf.len() {
            let moved = self.file.write_at(offset, &buf[written..]).await?;
            if moved == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!(
                        "file accepted only {} of {} bytes at block {block_index}",
                        written,
                        buf.len()
                    ),
                ));
            }
            written += moved;
            offset += moved as u64;
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

    /// Byte offset of a block-aligned transfer, validating its length.
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
        block_index
            .checked_mul(self.block_size as u64)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("block index {block_index} overflows a byte offset"),
                )
            })
    }
}
