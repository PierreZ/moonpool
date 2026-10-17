//! Block I/O over owned, aligned buffers, and block reads that treat a
//! failing medium as damaged bytes, CLSTORE's treatment of EIO: the block is
//! zero-filled, so whatever lived there fails its checksum and goes down the
//! one detection path.
//!
//! Every write and read here moves an [`AlignedBuf`] through
//! [`BlockFile`]'s zero-copy methods: the journal encodes a batch, a header
//! or the metainfo straight into the buffer the kernel transfers, and a
//! direct-I/O segment never copies it on the way.

use std::io;
use std::ops::Deref;

use moonpool_core::{AlignedBuf, BlockFile, StorageFile};

use crate::format::{BLOCK, BLOCK_U64, align_up, u64_of, usize_of};

/// A zeroed buffer of `len` bytes for the journal to encode into, on a
/// block boundary.
///
/// A block boundary meets the memory alignment of every direct-I/O file the
/// journal opens in practice (a device's is at most its logical block, and
/// a journal block is 4 KiB). A file that demanded more would still be
/// served: [`BlockFile`] copies a buffer that misses its alignment once into
/// one that meets it.
pub(crate) fn block_buf(len: usize) -> AlignedBuf {
    AlignedBuf::zeroed(len, BLOCK)
}

/// Whether a read error is the medium failing, rather than a request this
/// file can never satisfy.
fn is_media_error(error: &io::Error) -> bool {
    !matches!(
        error.kind(),
        io::ErrorKind::InvalidInput | io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
    )
}

/// Read whole blocks from `first_block` into `buf`, zero-filling every block
/// the medium fails to read. Returns the filled buffer.
pub(crate) async fn read_blocks<F: StorageFile>(
    file: &BlockFile<F>,
    first_block: u64,
    buf: AlignedBuf,
) -> io::Result<AlignedBuf> {
    let len = buf.len();
    match file.read_blocks_aligned(first_block, buf).await {
        Ok(buf) => return Ok(buf),
        Err(error) if !is_media_error(&error) => return Err(error),
        Err(_) => {}
    }
    // The failed read took the buffer: the block-by-block retry, the rare
    // path, reads into a new one.
    let mut buf = file.buffer(len / BLOCK)?;
    for (at, chunk) in buf.as_chunks_mut::<BLOCK>().0.iter_mut().enumerate() {
        match file.read_blocks(first_block + u64_of(at), chunk).await {
            Ok(()) => {}
            Err(error) if is_media_error(&error) => chunk.fill(0),
            Err(error) => return Err(error),
        }
    }
    Ok(buf)
}

/// Bytes read through whole blocks: the blocks' buffer and the range of it
/// that was asked for, read in place rather than copied out.
pub(crate) struct ReadBytes {
    buf: AlignedBuf,
    skip: usize,
    len: usize,
}

impl Deref for ReadBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf.as_slice()[self.skip..self.skip + self.len]
    }
}

/// Read `len` bytes at `offset` through whole blocks.
pub(crate) async fn read_bytes<F: StorageFile>(
    file: &BlockFile<F>,
    offset: u64,
    len: usize,
) -> io::Result<ReadBytes> {
    if len == 0 {
        return Ok(ReadBytes {
            buf: AlignedBuf::default(),
            skip: 0,
            len: 0,
        });
    }
    let start = offset / BLOCK_U64 * BLOCK_U64;
    let end = align_up(offset + u64_of(len), BLOCK_U64);
    let buf = file.buffer(usize_of((end - start) / BLOCK_U64))?;
    let buf = read_blocks(file, start / BLOCK_U64, buf).await?;
    Ok(ReadBytes {
        buf,
        skip: usize_of(offset - start),
        len,
    })
}

/// Write `buf` (a whole number of blocks) at block `first_block`, with no
/// copy. Returns the buffer.
pub(crate) async fn write_blocks<F: StorageFile>(
    file: &BlockFile<F>,
    first_block: u64,
    buf: AlignedBuf,
) -> io::Result<AlignedBuf> {
    assert!(buf.len().is_multiple_of(BLOCK), "writes are whole blocks");
    if buf.is_empty() {
        return Ok(buf);
    }
    file.write_blocks_aligned(first_block, buf).await
}

/// Write `buf` (a whole number of blocks) at block `first_block`, durable
/// on return: [`BlockFile::write_blocks_durable_aligned`]. Only these
/// blocks; a caller with another unsynced write in the file syncs it first.
pub(crate) async fn write_blocks_durable<F: StorageFile>(
    file: &BlockFile<F>,
    first_block: u64,
    buf: AlignedBuf,
) -> io::Result<AlignedBuf> {
    assert!(buf.len().is_multiple_of(BLOCK), "writes are whole blocks");
    if buf.is_empty() {
        return Ok(buf);
    }
    file.write_blocks_durable_aligned(first_block, buf).await
}
