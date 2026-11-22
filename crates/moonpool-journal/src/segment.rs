//! One segment file: allocated whole, header copy A, the persist log, the
//! entry log, and header copy B in the last block (see [`Geometry`]).
//!
//! A segment is created whole under a temporary name and published by a
//! rename ([`Segment::create`]), or adopted from a spare, a dead segment's
//! file, whose headers are synced before the rename ([`Segment::adopt`]),
//! so a segment file either does not exist or has its full size and both
//! headers. It never changes size again. Both logs only ever append, and
//! every batch starts on a fresh block in each, so no commit ever touches a
//! sector holding an acknowledged record or entry. (A record repair writes a
//! persist block again, but only when each record in it can be rebuilt from
//! its intact entry: see [`crate::RecordRepair`].) A recycled file's old
//! batches are older than its first batch: the scan reads them as stale
//! (see [`crate::spare`]).

use moonpool_core::{
    AlignedBuf, BlockFile, DirectIo, LocalStorageFile, LocalStorageProvider, OpenOptions,
};

use crate::format::{BLOCK, BLOCK_U64, Geometry, SegmentHeader, VERSION, usize_of};
use crate::io::{
    ReadBytes, block_buf, read_blocks, read_bytes, read_from_device, write_blocks,
    write_blocks_durable,
};
use crate::{JournalId, OpenError};

const PREFIX: &str = "seg-";
const SUFFIX: &str = ".wal";

/// `seg-00000000000000000042.wal`: named after the first batch it holds.
pub(crate) fn segment_name(first_batch: u64) -> String {
    format!("{PREFIX}{first_batch:020}{SUFFIX}")
}

/// The first batch a segment file name encodes, if it is one.
pub(crate) fn parse_segment_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    if digits.len() != 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Whether `name` is a segment a crashed creation left behind.
pub(crate) fn is_segment_leftover(name: &str) -> bool {
    name.strip_suffix(".tmp")
        .and_then(parse_segment_name)
        .is_some()
}

pub(crate) struct Segment<F> {
    pub first_batch: u64,
    /// The last batch before this segment (its header's link).
    pub prev_batch: u64,
    pub name: String,
    pub file: BlockFile<F>,
    pub geometry: Geometry,
    /// Where the next batch's persist records start (block-aligned).
    pub persist_next: u64,
    /// Where the next batch's entries start (block-aligned).
    pub entry_next: u64,
    /// Live index entries whose record or entry lives here.
    pub live: u64,
}

/// Allocate a new segment file whole, write both headers and sync it. The
/// range is reserved, not written: it reads as zeros, and a full disk fails
/// here, never in a commit.
async fn fill_new<F: LocalStorageFile>(
    file: F,
    geometry: Geometry,
    header: &SegmentHeader,
) -> Result<(), OpenError> {
    let file = BlockFile::new(file, BLOCK)?;
    file.get_ref()
        .allocate(geometry.total_blocks() * BLOCK_U64)
        .await?;
    write_headers(&file, header).await?;
    // Bytes and length alike: the file never changes size again.
    file.sync().await?;
    Ok(())
}

impl<F: LocalStorageFile> Segment<F> {
    /// Create a fresh, empty segment for batches from `first_batch` on.
    pub async fn create<P: LocalStorageProvider<File = F>>(
        provider: &P,
        dir: &str,
        journal: JournalId,
        first_batch: u64,
        prev_batch: u64,
        geometry: Geometry,
        direct_io: DirectIo,
    ) -> Result<Self, OpenError> {
        let name = segment_name(first_batch);
        let path = format!("{dir}/{name}");
        let temporary = format!("{path}.tmp");
        if provider.exists(&temporary).await? {
            provider.delete(&temporary).await?;
        }
        let file = provider
            .open(&temporary, OpenOptions::create_new_write().read(true))
            .await?;
        let header = SegmentHeader {
            journal,
            first_batch,
            prev_batch,
            geometry,
        };
        if let Err(error) = fill_new(file, geometry, &header).await {
            // Best effort: a half-made segment holds no batch, and on a
            // full disk its bytes are the space the next roll needs.
            // Whatever is left is deleted by the next open.
            let _ = provider.delete(&temporary).await;
            return Err(error);
        }
        provider.rename(&temporary, &path).await?;
        provider.sync_dir(dir).await?;
        let file = open_file(provider, &path, direct_io).await?;
        Ok(Self::fresh(first_batch, prev_batch, name, file, geometry))
    }

    /// Make the spare `spare` the segment for batches from `first_batch`
    /// on: write and sync its headers, rename it to its segment name, sync
    /// the directory. The spare is a dead segment's file, full size and
    /// synced, so this is one small durable write and a rename, with no
    /// allocation.
    ///
    /// The headers are durable before the name is: a crash leaves either the
    /// spare (every open deletes it, and no batch was written to it) or a
    /// whole, empty segment, exactly what a crash after `create` leaves.
    pub async fn adopt<P: LocalStorageProvider<File = F>>(
        provider: &P,
        dir: &str,
        spare: &str,
        header: SegmentHeader,
        direct_io: DirectIo,
    ) -> Result<Self, OpenError> {
        let SegmentHeader {
            first_batch,
            prev_batch,
            geometry,
            ..
        } = header;
        let name = segment_name(first_batch);
        let path = format!("{dir}/{name}");
        let spare = format!("{dir}/{spare}");
        let file = open_file(provider, &spare, direct_io).await?;
        if file.size_in_blocks().await? != geometry.total_blocks() {
            return Err(OpenError::BadSegment {
                name,
                reason: "the spare is not a whole segment",
            });
        }
        // Everything the spare holds is durable, so the headers are its one
        // unsynced write: making them durable alone is the full sync.
        write_headers_durable(&file, &header).await?;
        drop(file);
        // A power loss here leaves a spare with headers: still a spare.
        moonpool_buggify::hint!(
            "journal roll: spare headers synced, not renamed",
            ROLL_HINT_PROB
        )
        .await;
        provider.rename(&spare, &path).await?;
        // A power loss here leaves the spare's name, the segment's, or both:
        // two names of one file, which the next open tells apart by the
        // header ([`named_batches`]).
        moonpool_buggify::hint!(
            "journal roll: spare renamed, directory not synced",
            ROLL_HINT_PROB
        )
        .await;
        provider.sync_dir(dir).await?;
        let file = open_file(provider, &path, direct_io).await?;
        Ok(Self::fresh(first_batch, prev_batch, name, file, geometry))
    }

    fn fresh(
        first_batch: u64,
        prev_batch: u64,
        name: String,
        file: BlockFile<F>,
        geometry: Geometry,
    ) -> Self {
        Self {
            first_batch,
            prev_batch,
            name,
            persist_next: Geometry::PERSIST_START,
            entry_next: geometry.entry_start(),
            file,
            geometry,
            live: 0,
        }
    }

    /// Open an existing segment and check it is whole and ours. A damaged
    /// header copy is rewritten from its twin; a header block holds nothing
    /// else, so the rewrite reaches nothing acknowledged.
    pub async fn open<P: LocalStorageProvider<File = F>>(
        provider: &P,
        dir: &str,
        name: &str,
        first_batch: u64,
        journal: JournalId,
        geometry: Geometry,
        direct_io: DirectIo,
    ) -> Result<(Self, bool), OpenError> {
        let bad = |reason| OpenError::BadSegment {
            name: name.to_string(),
            reason,
        };
        let file = open_file(provider, &format!("{dir}/{name}"), direct_io).await?;
        if file.size_in_blocks().await? != geometry.total_blocks() {
            return Err(bad("wrong size"));
        }
        // Every read of this open, and of the scan after it, comes from
        // the device: a batch whose commit failed is not read as written.
        read_from_device(&file).await?;
        let headers = read_header_copies(&file, geometry.total_blocks()).await?;
        let a = SegmentHeader::decode(headers[0].as_slice());
        let b = SegmentHeader::decode(headers[1].as_slice());
        let (header, damaged) = match (a, b) {
            (Some(a), Some(b)) if a == b => (a, None),
            (Some(_), Some(_)) => return Err(bad("header copies disagree")),
            (Some(a), None) => (a, Some(1)),
            (None, Some(b)) => (b, Some(0)),
            (None, None) => {
                let other = SegmentHeader::other_version(headers[0].as_slice())
                    .or_else(|| SegmentHeader::other_version(headers[1].as_slice()));
                return Err(other.map_or_else(
                    || bad("no valid header"),
                    |found| OpenError::UnsupportedVersion {
                        found,
                        supported: VERSION,
                    },
                ));
            }
        };
        if header.journal != journal {
            return Err(OpenError::WrongJournal {
                found: header.journal.0,
            });
        }
        if header.first_batch != first_batch {
            return Err(bad("header names another first batch"));
        }
        if header.geometry != geometry {
            return Err(bad("header names another geometry"));
        }
        if let Some(copy) = damaged {
            write_header_copy_durable(&file, &header, copy).await?;
        }
        Ok((
            Self::fresh(
                first_batch,
                header.prev_batch,
                name.to_string(),
                file,
                geometry,
            ),
            damaged.is_some(),
        ))
    }

    /// Read both header copies again from the device and rewrite each one
    /// that is not this segment's header. Returns the copies rewritten. A
    /// header block holds nothing else, so a rewrite reaches nothing
    /// acknowledged.
    pub async fn scrub_headers(&self, journal: JournalId) -> std::io::Result<u32> {
        let header = SegmentHeader {
            journal,
            first_batch: self.first_batch,
            prev_batch: self.prev_batch,
            geometry: self.geometry,
        };
        // Every write to a segment is durable on return: nothing is dirty,
        // so the reads come from the device.
        self.file.get_ref().drop_cache().await?;
        let blocks = read_header_copies(&self.file, self.geometry.total_blocks()).await?;
        let mut rewritten = 0;
        for (copy, block) in blocks.iter().enumerate() {
            if SegmentHeader::decode(block.as_slice()) != Some(header) {
                // One copy at a time: a crash leaves the other one whole.
                write_header_copy_durable(&self.file, &header, copy).await?;
                rewritten += 1;
            }
        }
        Ok(rewritten)
    }

    /// The whole persist log, damaged blocks zero-filled.
    pub async fn read_persist_log(&self) -> std::io::Result<AlignedBuf> {
        let start = Geometry::PERSIST_START;
        let len = usize_of(self.geometry.persist_end() - start);
        read_blocks(
            &self.file,
            start / BLOCK_U64,
            self.file.buffer(len / BLOCK)?,
        )
        .await
    }

    pub async fn read_at(&self, offset: u64, len: usize) -> std::io::Result<ReadBytes> {
        read_bytes(&self.file, offset, len).await
    }

    /// Whether a batch of `persist_blocks` and `entry_bytes` fits after the
    /// last one.
    pub fn fits(&self, persist_blocks: u64, entry_bytes: u64) -> bool {
        self.persist_next + persist_blocks * BLOCK_U64 <= self.geometry.persist_end()
            && self.entry_next + entry_bytes <= self.geometry.entry_end()
    }

    /// Write `buf` at `offset`, with no copy; returns the buffer.
    pub async fn write(&self, offset: u64, buf: AlignedBuf) -> std::io::Result<AlignedBuf> {
        assert!(offset.is_multiple_of(BLOCK_U64), "writes start on a block");
        write_blocks(&self.file, offset / BLOCK_U64, buf).await
    }

    /// [`write`](Self::write), durable on return: one durable write where
    /// `write` and `sync` are two calls. Only `buf`'s range is made
    /// durable, so the segment must hold no other unsynced write; data-only
    /// durability is enough because a segment never changes size.
    pub async fn write_durable(&self, offset: u64, buf: AlignedBuf) -> std::io::Result<AlignedBuf> {
        assert!(offset.is_multiple_of(BLOCK_U64), "writes start on a block");
        write_blocks_durable(&self.file, offset / BLOCK_U64, buf).await
    }

    pub async fn sync(&self) -> std::io::Result<()> {
        self.file.sync_data().await
    }
}

/// How often an active roll point fires: a roll is rare, so the commit's
/// rate.
const ROLL_HINT_PROB: f64 = 0.25;

/// The first batches the header copies of the file at `path` name for
/// `journal`: none when neither copy decodes or both name another journal.
///
/// A crash between a rename and its directory sync can keep both names, one
/// file behind them. A spare whose header names a segment the directory
/// lists is such a second name: writing into it writes into that segment.
pub(crate) async fn named_batches<P: LocalStorageProvider>(
    provider: &P,
    path: &str,
    journal: JournalId,
    direct_io: DirectIo,
) -> Result<Vec<u64>, OpenError> {
    let file = open_file(provider, path, direct_io).await?;
    let blocks = file.size_in_blocks().await?;
    if blocks < 2 {
        return Ok(Vec::new());
    }
    // Defensive: a roll whose header write failed poisons the journal, but
    // it may have left its headers in the cache only. The device's are the
    // ones a power loss keeps, so they decide whether this spare aliases a
    // live segment.
    read_from_device(&file).await?;
    let headers = read_header_copies(&file, blocks).await?;
    Ok(headers
        .iter()
        .filter_map(|block| SegmentHeader::decode(block.as_slice()))
        .filter(|header| header.journal == journal)
        .map(|header| header.first_batch)
        .collect())
}

/// The block of header copy `copy` in a file of `blocks` blocks: copy A
/// (0) is the first block, copy B (1) the last.
pub(crate) fn header_block(blocks: u64, copy: usize) -> u64 {
    if copy == 0 { 0 } else { blocks - 1 }
}

/// Read both header copies of a file of `blocks` blocks, damaged blocks
/// zero-filled.
async fn read_header_copies<F: LocalStorageFile>(
    file: &BlockFile<F>,
    blocks: u64,
) -> std::io::Result<[AlignedBuf; 2]> {
    let a = read_blocks(file, header_block(blocks, 0), file.buffer(1)?).await?;
    let b = read_blocks(file, header_block(blocks, 1), file.buffer(1)?).await?;
    Ok([a, b])
}

/// One header copy, encoded in place.
fn header_copy(header: &SegmentHeader) -> AlignedBuf {
    let mut block = block_buf(BLOCK);
    header.encode(block.as_mut_slice());
    block
}

/// Write both header copies of a segment, A then B, not synced.
async fn write_headers<F: LocalStorageFile>(
    file: &BlockFile<F>,
    header: &SegmentHeader,
) -> std::io::Result<()> {
    let blocks = header.geometry.total_blocks();
    for copy in 0..2 {
        write_blocks(file, header_block(blocks, copy), header_copy(header)).await?;
    }
    Ok(())
}

/// Write header copy `copy`, durable on return.
async fn write_header_copy_durable<F: LocalStorageFile>(
    file: &BlockFile<F>,
    header: &SegmentHeader,
    copy: usize,
) -> std::io::Result<()> {
    let block = header_block(header.geometry.total_blocks(), copy);
    write_blocks_durable(file, block, header_copy(header)).await?;
    Ok(())
}

/// Both header copies, each durable before the next is written, so a crash
/// tears at most one of them.
async fn write_headers_durable<F: LocalStorageFile>(
    file: &BlockFile<F>,
    header: &SegmentHeader,
) -> std::io::Result<()> {
    for copy in 0..2 {
        write_header_copy_durable(file, header, copy).await?;
    }
    Ok(())
}

async fn open_file<P: LocalStorageProvider>(
    provider: &P,
    path: &str,
    direct_io: DirectIo,
) -> std::io::Result<BlockFile<P::File>> {
    let file = provider
        .open(path, OpenOptions::read_write().direct_io(direct_io))
        .await?;
    BlockFile::new(file, BLOCK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        assert_eq!(parse_segment_name(&segment_name(42)), Some(42));
        assert!(is_segment_leftover(&format!("{}.tmp", segment_name(7))));
        assert_eq!(parse_segment_name("seg-1.wal"), None);
    }
}
