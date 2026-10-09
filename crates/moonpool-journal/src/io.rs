//! Block reads that treat a failing medium as damaged bytes, CLSTORE's
//! treatment of EIO: the block is zero-filled, so whatever lived there fails
//! its checksum and goes down the one detection path.

use std::io;

use moonpool_core::{BlockFile, StorageFile};

use crate::format::{BLOCK, BLOCK_U64, align_up, u64_of, usize_of};

/// Whether a read error is the medium failing, rather than a request this
/// file can never satisfy.
fn is_media_error(error: &io::Error) -> bool {
    !matches!(
        error.kind(),
        io::ErrorKind::InvalidInput | io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
    )
}

/// Read whole blocks from `first_block` into `buf`, zero-filling every block
/// the medium fails to read.
pub(crate) async fn read_blocks<F: StorageFile>(
    file: &BlockFile<F>,
    first_block: u64,
    buf: &mut [u8],
) -> io::Result<()> {
    match file.read_blocks(first_block, buf).await {
        Ok(()) => return Ok(()),
        Err(error) if !is_media_error(&error) => return Err(error),
        Err(_) => {}
    }
    for (at, chunk) in buf.as_chunks_mut::<BLOCK>().0.iter_mut().enumerate() {
        match file.read_blocks(first_block + u64_of(at), chunk).await {
            Ok(()) => {}
            Err(error) if is_media_error(&error) => chunk.fill(0),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Read `len` bytes at `offset` through whole blocks.
pub(crate) async fn read_bytes<F: StorageFile>(
    file: &BlockFile<F>,
    offset: u64,
    len: usize,
) -> io::Result<Vec<u8>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let start = offset / BLOCK_U64 * BLOCK_U64;
    let end = align_up(offset + u64_of(len), BLOCK_U64);
    let mut buf = file.buffer(usize_of((end - start) / BLOCK_U64))?;
    read_blocks(file, start / BLOCK_U64, buf.as_mut_slice()).await?;
    let skip = usize_of(offset - start);
    Ok(buf.as_slice()[skip..skip + len].to_vec())
}

/// Write `bytes` (a whole number of blocks) at block `first_block`.
pub(crate) async fn write_blocks<F: StorageFile>(
    file: &BlockFile<F>,
    first_block: u64,
    bytes: &[u8],
) -> io::Result<()> {
    assert!(bytes.len().is_multiple_of(BLOCK), "writes are whole blocks");
    if bytes.is_empty() {
        return Ok(());
    }
    let mut buf = file.buffer(bytes.len() / BLOCK)?;
    buf.as_mut_slice().copy_from_slice(bytes);
    file.write_blocks(first_block, buf.as_slice()).await
}
