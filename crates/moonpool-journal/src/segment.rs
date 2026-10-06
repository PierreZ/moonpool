//! One segment file: preallocated and zero-filled, two header copies, then
//! the persist log and the entry log (see [`Geometry`]).
//!
//! A segment is created whole under a temporary name and published by a
//! rename, so a segment file either does not exist or has its full size and
//! both headers. It never changes size again, so `fdatasync` never has file
//! metadata to write. Both logs only ever append, and every batch starts on
//! a fresh block in each, so no write ever touches a sector holding an
//! acknowledged record or entry.

use moonpool_core::{BlockFile, DirectIo, OpenOptions, StorageFile, StorageProvider};

use crate::format::{BLOCK, BLOCK_U64, Geometry, SegmentHeader, usize_of};
use crate::io::{read_blocks, read_bytes, write_blocks};
use crate::{JournalId, OpenError};

/// Bytes zeroed per write while preallocating a segment.
const FILL_BLOCKS: u64 = 256;

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

impl<F: StorageFile> Segment<F> {
    /// Create a fresh, empty segment for batches from `first_batch` on.
    pub async fn create<P: StorageProvider<File = F>>(
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
        let file = BlockFile::new(file, BLOCK)?;
        let total = geometry.total_blocks();
        let mut block = 0;
        while block < total {
            let count = FILL_BLOCKS.min(total - block);
            let mut chunk = vec![0u8; usize_of(count * BLOCK_U64)];
            if block == 0 {
                let header = SegmentHeader {
                    journal,
                    first_batch,
                    prev_batch,
                    geometry,
                };
                header.encode(&mut chunk[..BLOCK]);
                header.encode(&mut chunk[BLOCK..2 * BLOCK]);
            }
            write_blocks(&file, block, &chunk).await?;
            block += count;
        }
        // Bytes and length alike: the file never changes size again.
        file.sync().await?;
        drop(file);
        provider.rename(&temporary, &path).await?;
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
    pub async fn open<P: StorageProvider<File = F>>(
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
        let mut headers = file.buffer(2)?;
        read_blocks(&file, 0, headers.as_mut_slice()).await?;
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
            let mut copy = vec![0u8; BLOCK];
            header.encode(&mut copy);
            write_blocks(&file, block, &copy).await?;
            file.sync_data().await?;
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
    pub async fn read_persist_log(&self) -> std::io::Result<Vec<u8>> {
        let start = Geometry::PERSIST_START;
        let len = usize_of(self.geometry.persist_end() - start);
        let mut buf = self.file.buffer(len / BLOCK)?;
        read_blocks(&self.file, start / BLOCK_U64, buf.as_mut_slice()).await?;
        Ok(buf.as_slice().to_vec())
    }

    pub async fn read_at(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        read_bytes(&self.file, offset, len).await
    }

    /// Whether a batch of `persist_blocks` and `entry_bytes` fits after the
    /// last one.
    pub fn fits(&self, persist_blocks: u64, entry_bytes: u64) -> bool {
        self.persist_next + persist_blocks * BLOCK_U64 <= self.geometry.persist_end()
            && self.entry_next + entry_bytes <= self.geometry.entry_end()
    }

    pub async fn write(&self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        assert!(offset.is_multiple_of(BLOCK_U64), "writes start on a block");
        write_blocks(&self.file, offset / BLOCK_U64, bytes).await
    }

    pub async fn sync(&self) -> std::io::Result<()> {
        self.file.sync_data().await
    }
}

async fn open_file<P: StorageProvider>(
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
