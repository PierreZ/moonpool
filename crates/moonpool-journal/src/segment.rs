//! One segment file: preallocated and zero-filled, two header copies, then
//! the persist log and the entry log (see [`Geometry`]).
//!
//! A segment is created whole under a temporary name and published by a
//! rename ([`Segment::create`]), or adopted from a prepared spare whose
//! headers are synced before the rename ([`Segment::adopt`]), so a segment
//! file either does not exist or has its full size and both headers. It never changes size again, so `fdatasync` never has file
//! metadata to write. Both logs only ever append, and every batch starts on
//! a fresh block in each, so no write ever touches a sector holding an
//! acknowledged record or entry.

use moonpool_core::{
    AlignedBuf, BlockFile, DirectIo, LocalStorageFile, LocalStorageProvider, OpenOptions,
};

use crate::format::{BLOCK, BLOCK_U64, Geometry, SegmentHeader, usize_of};
use crate::io::{
    ReadBytes, block_buf, read_blocks, read_bytes, write_blocks, write_blocks_durable,
};
use crate::spare::zero_fill;
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

/// Zero-fill a new segment file whole, write both headers and sync it.
async fn fill_new<F: LocalStorageFile>(
    file: F,
    geometry: Geometry,
    header: &SegmentHeader,
) -> Result<(), OpenError> {
    let file = BlockFile::new(file, BLOCK)?;
    zero_fill(&file, geometry.total_blocks(), &|| false).await?;
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
            // Best effort: a half-filled segment holds no batch, and on a
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

    /// Make the prepared spare `spare` the segment for batches from
    /// `first_batch` on: write and sync its headers, rename it to its
    /// segment name, sync the directory. The spare is full size, zero-filled
    /// and synced, so this is two small syncs where [`create`](Self::create)
    /// writes and syncs a whole segment.
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
        // The spare is zero-filled and synced whole, so the headers are its
        // one unsynced write: making them durable alone is the full sync.
        write_headers_durable(&file, &header).await?;
        drop(file);
        // A power loss here leaves a spare with headers: still a spare.
        moonpool_buggify::hint!(
            "journal roll: spare headers synced, not renamed",
            ROLL_HINT_PROB
        )
        .await;
        provider.rename(&spare, &path).await?;
        // A power loss here leaves the spare's name, the segment's, or both.
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
        let headers = read_blocks(&file, 0, file.buffer(2)?).await?;
        let a = SegmentHeader::decode(&headers.as_slice()[..BLOCK]);
        let b = SegmentHeader::decode(&headers.as_slice()[BLOCK..]);
        let (header, damaged) = match (a, b) {
            (Some(a), Some(b)) if a == b => (a, None),
            (Some(_), Some(_)) => return Err(bad("header copies disagree")),
            (Some(a), None) => (a, Some(1)),
            (None, Some(b)) => (b, Some(0)),
            (None, None) => return Err(bad("no valid header")),
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
        if let Some(block) = damaged {
            let mut copy = block_buf(BLOCK);
            header.encode(copy.as_mut_slice());
            write_blocks_durable(&file, block, copy).await?;
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

    /// Whether either log is at least half full: time to prepare the next
    /// segment.
    pub fn half_full(&self) -> bool {
        let persist_used = self.persist_next - Geometry::PERSIST_START;
        let entry_used = self.entry_next - self.geometry.entry_start();
        persist_used * 2 >= self.geometry.persist_end() - Geometry::PERSIST_START
            || entry_used * 2 >= self.geometry.entry_end() - self.geometry.entry_start()
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

/// Write both header copies of a segment (blocks 0 and 1).
async fn write_headers<F: LocalStorageFile>(
    file: &BlockFile<F>,
    header: &SegmentHeader,
) -> std::io::Result<()> {
    write_blocks(file, 0, header_blocks(header)).await?;
    Ok(())
}

/// [`write_headers`], durable on return.
async fn write_headers_durable<F: LocalStorageFile>(
    file: &BlockFile<F>,
    header: &SegmentHeader,
) -> std::io::Result<()> {
    write_blocks_durable(file, 0, header_blocks(header)).await?;
    Ok(())
}

/// Both header copies, encoded in place: blocks 0 and 1.
fn header_blocks(header: &SegmentHeader) -> AlignedBuf {
    let mut blocks = block_buf(2 * BLOCK);
    header.encode(&mut blocks[..BLOCK]);
    header.encode(&mut blocks[BLOCK..]);
    blocks
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
