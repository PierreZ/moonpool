//! One segment file: a preallocated, zero-filled [`BlockFile`] holding a
//! header, a slot table, and a data region of contiguous entries.
//!
//! In memory a segment keeps one [`Rec`] per index it holds — the slot the
//! startup scan verified or rebuilt — so a read computes where its entry is
//! and never reads the slot table again.
//!
//! Every append batch starts on a fresh [`BLOCK`]: the data write never
//! touches a block an earlier sync made durable, so no crash during an
//! append can damage an acknowledged entry, whatever the disk does to the
//! sectors being written.

use std::io;
use std::ops::Range;

use moonpool_core::{BlockFile, DirectIo, OpenOptions, StorageFile, StorageProvider};

use crate::layout::{
    BLOCK, BLOCK_U64, ENTRY_HEADER_SIZE, EntryHeader, Geometry, Header, SLOT_SIZE,
    SLOT_TABLE_OFFSET, Slot, SlotState, Tag, align_down, align_up, encode_entry, entry_crc_ok,
    entry_size,
};
use crate::scan::{Decision, Found, Rec, decide};
use crate::{JournalError, Record};

/// Bytes read at a time while the recovery scan walks the data region.
const SCAN_WINDOW: u64 = 1 << 20;

/// Bytes zeroed per write while preallocating a segment.
const FILL_CHUNK: u64 = 1 << 20;

const PREFIX: &str = "seg-";
const SUFFIX: &str = ".wal";

/// An entry read back from the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its log index.
    pub index: u64,
    /// The epoch (term) it was written with.
    pub epoch: u64,
    /// The caller's identity it was written with.
    pub tag: Tag,
    /// The caller's bytes.
    pub payload: Vec<u8>,
}

impl Entry {
    /// The entry's identity: what its slot records.
    #[must_use]
    pub fn id(&self) -> EntryId {
        EntryId {
            index: self.index,
            epoch: self.epoch,
            tag: self.tag,
        }
    }
}

/// An entry's identity as its slot records it, far from the entry: known
/// even when the entry's own bytes are lost, which is what lets a replicated
/// caller re-fetch exactly that entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId {
    /// Its log index.
    pub index: u64,
    /// The epoch (term) it was written with.
    pub epoch: u64,
    /// The caller's identity it was written with.
    pub tag: Tag,
}

/// The file name of the segment whose first index is `first`:
/// `seg-00000000000001048576.wal`.
pub(crate) fn segment_name(first: u64) -> String {
    format!("{PREFIX}{first:020}{SUFFIX}")
}

/// The first index a segment file name encodes, if it is one.
pub(crate) fn parse_segment_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    if digits.len() != 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Whether `name` is a segment being created that a crash left behind.
pub(crate) fn is_segment_leftover(name: &str) -> bool {
    name.strip_suffix(".tmp")
        .and_then(parse_segment_name)
        .is_some()
}

pub(crate) fn segment_path(dir: &str, first: u64) -> String {
    format!("{dir}/{}", segment_name(first))
}

/// What the recovery of one segment found.
#[derive(Debug, Default)]
pub(crate) struct SegmentRecovery {
    pub corrupt: Vec<EntryId>,
    pub slots_rewritten: usize,
    pub header_repaired: bool,
    /// The log ends in this segment (before its bound, for a sealed one).
    pub ended: bool,
    /// Something past the end was discarded and zeroed.
    pub torn: bool,
}

pub(crate) struct Segment<F> {
    pub first: u64,
    file: BlockFile<F>,
    geometry: Geometry,
    recs: Vec<Rec>,
    /// First byte past the last kept entry. The next batch starts at the
    /// block boundary at or after it.
    data_end: u64,
}

fn u64_len(len: usize) -> u64 {
    u64::try_from(len).expect("usize fits u64")
}

fn usize_len(len: u64) -> io::Result<usize> {
    usize::try_from(len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "length overflows usize"))
}

/// Whether a read error is the medium failing (the paper's EIO), rather than
/// a request this file can never satisfy.
fn is_media_error(error: &io::Error) -> bool {
    !matches!(
        error.kind(),
        io::ErrorKind::InvalidInput | io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
    )
}

/// Read whole blocks from `first_block` into `buf`. A block the medium fails
/// to read is zero-filled — so its checksum fails, CLSTORE's treatment of
/// EIO — and flagged in the returned vector.
async fn read_blocks_tolerant<F: StorageFile>(
    file: &BlockFile<F>,
    first_block: u64,
    buf: &mut [u8],
) -> io::Result<Vec<bool>> {
    let blocks = buf.len() / BLOCK;
    match file.read_blocks(first_block, buf).await {
        Ok(()) => return Ok(vec![false; blocks]),
        Err(error) if !is_media_error(&error) => return Err(error),
        Err(_) => {}
    }
    let mut failed = vec![false; blocks];
    for (at, chunk) in buf.chunks_exact_mut(BLOCK).enumerate() {
        match file.read_blocks(first_block + u64_len(at), chunk).await {
            Ok(()) => {}
            Err(error) if is_media_error(&error) => {
                chunk.fill(0);
                failed[at] = true;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(failed)
}

/// Read `len` bytes at `offset` through whole blocks, zero-filling what the
/// medium fails to read. Returns the bytes and whether any read failed.
async fn read_bytes<F: StorageFile>(
    file: &BlockFile<F>,
    offset: u64,
    len: usize,
) -> io::Result<(Vec<u8>, bool)> {
    if len == 0 {
        return Ok((Vec::new(), false));
    }
    let start = align_down(offset, BLOCK_U64);
    let end = align_up(offset + u64_len(len), BLOCK_U64);
    let mut buf = file.buffer(usize_len((end - start) / BLOCK_U64)?)?;
    let failed = read_blocks_tolerant(file, start / BLOCK_U64, buf.as_mut_slice()).await?;
    let skip = usize_len(offset - start)?;
    Ok((
        buf.as_slice()[skip..skip + len].to_vec(),
        failed.contains(&true),
    ))
}

/// A read-ahead window over the data region, for the sequential scan.
struct Window<'a, F> {
    file: &'a BlockFile<F>,
    limit: u64,
    start: u64,
    bytes: Vec<u8>,
}

impl<'a, F: StorageFile> Window<'a, F> {
    fn new(file: &'a BlockFile<F>, limit: u64) -> Self {
        Self {
            file,
            limit,
            start: 0,
            bytes: Vec::new(),
        }
    }

    /// `len` bytes at `offset`, or `None` past the end of the segment.
    async fn get(&mut self, offset: u64, len: usize) -> io::Result<Option<&[u8]>> {
        let end = offset + u64_len(len);
        if end > self.limit {
            return Ok(None);
        }
        if offset < self.start || end > self.start + u64_len(self.bytes.len()) {
            let start = align_down(offset, BLOCK_U64);
            let stop = align_up(end, BLOCK_U64)
                .max(start + SCAN_WINDOW)
                .min(self.limit);
            self.bytes = read_bytes(self.file, start, usize_len(stop - start)?)
                .await?
                .0;
            self.start = start;
        }
        let skip = usize_len(offset - self.start)?;
        Ok(Some(&self.bytes[skip..skip + len]))
    }
}

impl<F: StorageFile> Segment<F> {
    /// Create a fresh segment: zero-filled to its full size with both header
    /// copies written and every slot formatted with its reserved record,
    /// synced, and only then published under its name — so a segment file
    /// either does not exist or is whole.
    pub async fn create<P: StorageProvider<File = F>>(
        provider: &P,
        dir: &str,
        first: u64,
        geometry: Geometry,
        direct_io: DirectIo,
    ) -> Result<Self, JournalError> {
        let path = segment_path(dir, first);
        let temporary = format!("{path}.tmp");
        if provider.exists(&temporary).await? {
            provider.delete(&temporary).await?;
        }
        let file = provider
            .open(&temporary, OpenOptions::create_new_write().read(true))
            .await?;
        let blocks = BlockFile::new(file, BLOCK)?;
        let chunk_blocks = FILL_CHUNK / BLOCK_U64;
        let total_blocks = geometry.segment_size / BLOCK_U64;
        let mut zeros = blocks.buffer(usize_len(chunk_blocks)?)?;
        let mut block = 0;
        while block < total_blocks {
            let count = chunk_blocks.min(total_blocks - block);
            let buf = &mut zeros.as_mut_slice()[..usize_len(count * BLOCK_U64)?];
            if block == 0 {
                let header = Header::new(first, geometry);
                header.encode(&mut buf[..BLOCK]);
                header.encode(&mut buf[BLOCK..2 * BLOCK]);
            }
            let formatted = format_slots(first, geometry, block, buf);
            blocks.write_blocks(block, buf).await?;
            if block == 0 || formatted {
                buf.fill(0);
            }
            block += count;
        }
        // Length and bytes alike: after this, fdatasync never has metadata
        // to write, because the file never changes size again.
        blocks.sync().await?;
        drop(blocks);
        provider.rename(&temporary, &path).await?;
        provider.sync_dir(dir).await?;
        let file = Self::open_file(provider, &path, direct_io).await?;
        Ok(Self::empty(first, file, geometry))
    }

    /// A segment handle over `file` with no records loaded yet.
    fn empty(first: u64, file: BlockFile<F>, geometry: Geometry) -> Self {
        Self {
            first,
            file,
            geometry,
            recs: Vec::new(),
            data_end: geometry.data_start,
        }
    }

    async fn open_file<P: StorageProvider<File = F>>(
        provider: &P,
        path: &str,
        direct_io: DirectIo,
    ) -> Result<BlockFile<F>, JournalError> {
        let file = provider
            .open(path, OpenOptions::read_write().direct_io(direct_io))
            .await?;
        Ok(BlockFile::new(file, BLOCK)?)
    }

    /// Open an existing segment and run the recovery scan over it.
    ///
    /// `next_first` is the first index of the following segment, when one
    /// exists: the walk stops there, and must get there — a sealed segment
    /// that ends short is [`JournalError::SegmentGap`]. Wherever the log turns out to end — in
    /// the last segment, or earlier in a sealed one — everything past the
    /// end is zeroed before the journal accepts an append, so "empty" keeps
    /// meaning all zeros.
    pub async fn recover<P: StorageProvider<File = F>>(
        provider: &P,
        dir: &str,
        first: u64,
        geometry: Geometry,
        direct_io: DirectIo,
        next_first: Option<u64>,
    ) -> Result<(Self, SegmentRecovery), JournalError> {
        let path = segment_path(dir, first);
        let file = Self::open_file(provider, &path, direct_io).await?;
        let actual = file.get_ref().size().await?;
        if actual != geometry.segment_size {
            return Err(JournalError::SegmentSize {
                first_index: first,
                expected: geometry.segment_size,
                actual,
            });
        }
        let mut segment = Self::empty(first, file, geometry);
        let mut report = SegmentRecovery {
            header_repaired: segment.check_header().await?,
            ..SegmentRecovery::default()
        };

        let (found, table) = segment.walk(next_first).await?;
        let Decision {
            recs,
            rewrite_slots,
            corrupt,
            ended,
            torn,
        } = decide(first, &found)?;
        // A sealed segment holds every index up to the next one's first: a
        // segment that ends short is a hole in the log, never its end, and
        // nothing is written before saying so.
        if let Some(next) = next_first {
            let end = first + u64_len(recs.len());
            if end != next {
                return Err(JournalError::SegmentGap { end, next });
            }
        }
        segment.recs = recs;
        segment.data_end = segment.recs.last().map_or(geometry.data_start, |rec| {
            u64::from(rec.slot.offset) + entry_size(rec.slot.length)
        });
        report.corrupt = corrupt;
        report.slots_rewritten = rewrite_slots.len();
        report.ended = ended || next_first.is_none();
        report.torn = torn;

        // Rewrite the lost slots of intact entries.
        let mut dirty: Vec<u64> = rewrite_slots.iter().map(|index| index - first).collect();
        let mut needs_cut = false;
        if report.ended {
            // Clean up after the truncation, slots first: every slot past the
            // end gets its reserved record back, synced before any entry
            // bytes are zeroed, so a crash in between can never leave an
            // identifier beside a zeroed entry.
            let kept = u64_len(segment.recs.len());
            let stale: Vec<u64> = (kept..u64::from(geometry.slot_count))
                .filter(|rel| {
                    let at = usize_len(*rel).expect("slot fits") * SLOT_SIZE;
                    Slot::decode(&table[at..at + SLOT_SIZE], first + rel) != SlotState::Reserved
                })
                .collect();
            report.torn |= !stale.is_empty();
            dirty.extend(stale);
            // Anything discarded in the block holding the end stays there
            // (the block is never rewritten): the last kept slot says so.
            let next = segment.next_batch_at();
            let (rest, _) = read_bytes(
                &segment.file,
                segment.data_end,
                usize_len(next - segment.data_end)?,
            )
            .await?;
            needs_cut = report.torn || rest.iter().any(|b| *b != 0);
        }
        if !dirty.is_empty() || report.header_repaired {
            segment.write_slots(&dirty).await?;
            segment.file.sync_data().await?;
        }
        if report.ended {
            // The cut mark goes out only once the reserved records past the
            // end are durable: a crash that tore the two together could
            // leave the mark forbidding a continuation that a damaged,
            // never-reset identifier still needs.
            let mut wrote = false;
            if needs_cut && let Some(rel) = segment.mark_cut() {
                segment.write_slots(&[rel]).await?;
                wrote = true;
            }
            // The entry bytes past the end, from the next block on: if
            // anything but zeros starts there, whatever the crash left is
            // zeroed too, so a stale entry that still passes its CRC can
            // never be picked up through a lost slot later. The block
            // holding the end also holds kept entries and is never
            // rewritten; past the end its bytes are inert, since every slot
            // there is reserved and the walk never looks behind a reserved
            // slot.
            let next = segment.next_batch_at();
            let (terminator, _) = read_bytes(
                &segment.file,
                next,
                ENTRY_HEADER_SIZE.min(usize_len(geometry.segment_size - next)?),
            )
            .await?;
            if report.torn || terminator.iter().any(|b| *b != 0) {
                let wiped = segment.zero_data(next, geometry.segment_size, true).await?;
                wrote |= wiped;
                report.torn |= wiped;
            }
            if wrote {
                segment.file.sync_data().await?;
            }
        }
        Ok((segment, report))
    }

    /// Verify the two header copies, repairing one from the other.
    /// Returns whether a copy was rewritten (the caller syncs).
    async fn check_header(&self) -> Result<bool, JournalError> {
        let mut buf = self.file.buffer(2)?;
        read_blocks_tolerant(&self.file, 0, buf.as_mut_slice()).await?;
        let expected = Header::new(self.first, self.geometry);
        let copies = [
            Header::decode(&buf.as_slice()[..BLOCK]),
            Header::decode(&buf.as_slice()[BLOCK..]),
        ];
        let good = copies.map(|copy| copy == Some(expected));
        if !good[0] && !good[1] {
            return Err(JournalError::BadSegmentHeader {
                first_index: self.first,
            });
        }
        if good[0] && good[1] {
            return Ok(false);
        }
        let damaged = u64::from(good[0]);
        let mut block = self.file.buffer(1)?;
        expected.encode(block.as_mut_slice());
        self.file.write_blocks(damaged, block.as_slice()).await?;
        Ok(true)
    }

    /// Walk the indexes in order. Entry *i* is located through slot *i*, or —
    /// when that slot is damaged — after entry *i − 1* (see [`locate`]). The
    /// walk stops at `next_first`, at a reserved slot (the end of the log),
    /// or at the first index with neither an intact entry nor an identifier
    /// (a torn end in the last batch, a double fault before it). Returns
    /// what it found and the raw slot table.
    async fn walk(&self, next_first: Option<u64>) -> Result<(Vec<Found>, Vec<u8>), JournalError> {
        let geometry = self.geometry;
        let mut table = self.file.buffer(usize_len(geometry.slot_table_blocks())?)?;
        let failed = read_blocks_tolerant(
            &self.file,
            SLOT_TABLE_OFFSET / BLOCK_U64,
            table.as_mut_slice(),
        )
        .await?;
        let table_len = usize_len(u64::from(geometry.slot_count) * SLOT_SIZE as u64)?;
        let table = table.as_slice()[..table_len].to_vec();
        // The slot table bounds the walk; the next segment's name only
        // cross-checks it (a sealed segment must reach it exactly).
        let limit = next_first.map_or(u64::from(geometry.slot_count), |next| {
            (next - self.first).min(u64::from(geometry.slot_count))
        });

        let decoded: Vec<SlotState> = (0..limit)
            .map(|rel| {
                let at = usize_len(rel).expect("slot fits") * SLOT_SIZE;
                // An identifier the medium cannot read is damaged.
                if failed[at / BLOCK] {
                    SlotState::Bad
                } else {
                    Slot::decode(&table[at..at + SLOT_SIZE], self.first + rel)
                }
            })
            .collect();
        // Whether a later batch starts after each index: an identifier with
        // the batch-start flag further on, before any reserved one. A later
        // batch was appended after this index's batch synced, so damage here
        // is not a torn last batch. Identifiers past a reserved one are
        // left over from a cut a crash interrupted, and prove nothing. Only
        // the tail segment can hold the log's last batch: a segment is
        // sealed after its last batch synced.
        let mut later_batch = vec![false; decoded.len()];
        let mut ahead = false;
        for (rel, slot) in decoded.iter().enumerate().rev() {
            later_batch[rel] = ahead;
            match slot {
                SlotState::Reserved => ahead = false,
                SlotState::Valid(slot) if slot.batch_start => ahead = true,
                _ => {}
            }
        }
        let is_tail = next_first.is_none();

        let mut window = Window::new(&self.file, geometry.segment_size);
        let mut found = Vec::new();
        // The first entry of a segment starts the data region.
        let mut prev_end = geometry.data_start;
        // The previous index's slot says its batch was cut after it: no
        // entry continues it.
        let mut prev_cut = false;
        for (rel, slot) in decoded.into_iter().enumerate() {
            let index = self.first + u64_len(rel);
            let entry = match slot {
                SlotState::Valid(slot) => {
                    let offset = u64::from(slot.offset);
                    probe(&mut window, geometry, offset, index, Some(slot))
                        .await?
                        .map(|header| (offset, header))
                }
                SlotState::Reserved => None,
                SlotState::Bad => locate(&mut window, geometry, prev_end, !prev_cut, index).await?,
            };
            prev_cut = matches!(slot, SlotState::Valid(slot) if slot.cut);
            prev_end = match (entry, slot) {
                (Some((offset, header)), _) => offset + entry_size(header.length),
                (None, SlotState::Valid(slot)) => u64::from(slot.offset) + entry_size(slot.length),
                (None, _) => prev_end,
            };
            found.push(Found {
                slot,
                entry,
                in_last_batch: is_tail && !later_batch[rel],
            });
            // A reserved slot ends the log; an index with neither an intact
            // entry nor an identifier ends it too (torn) or is a double
            // fault. Either way the walk has no way to go on.
            if slot == SlotState::Reserved || (entry.is_none() && slot == SlotState::Bad) {
                break;
            }
        }
        Ok((found, table))
    }

    /// Rewrite the slot-table blocks holding `rels` from the in-memory
    /// records (reserved records past the kept range). Does not sync.
    async fn write_slots(&self, rels: &[u64]) -> Result<(), JournalError> {
        let per_block = BLOCK_U64 / SLOT_SIZE as u64;
        let mut blocks: Vec<u64> = rels.iter().map(|rel| rel / per_block).collect();
        blocks.sort_unstable();
        blocks.dedup();
        let mut run_start = 0;
        while run_start < blocks.len() {
            let mut run_end = run_start + 1;
            while run_end < blocks.len() && blocks[run_end] == blocks[run_end - 1] + 1 {
                run_end += 1;
            }
            let first_block = blocks[run_start];
            let mut buf = self.file.buffer(run_end - run_start)?;
            for (at, bytes) in buf.as_mut_slice().chunks_exact_mut(SLOT_SIZE).enumerate() {
                let rel = usize_len(first_block * per_block)? + at;
                match self.recs.get(rel) {
                    Some(rec) => rec.slot.encode(bytes),
                    None if rel < self.geometry.slot_count as usize => {
                        Slot::encode_reserved(self.first + u64_len(rel), bytes);
                    }
                    // Past the table, in its last block: never read.
                    None => bytes.fill(0),
                }
            }
            self.file
                .write_blocks(SLOT_TABLE_OFFSET / BLOCK_U64 + first_block, buf.as_slice())
                .await?;
            run_start = run_end;
        }
        Ok(())
    }

    /// Zero the whole blocks of `[from, to)` of the data region. `from` is a
    /// block boundary, so no kept byte shares a block with what is zeroed:
    /// zeroing never rewrites a sector an earlier sync made durable for a
    /// kept entry. With `only_dirty`, windows already zero are left
    /// untouched. Returns whether anything was written. Does not sync.
    async fn zero_data(&self, from: u64, to: u64, only_dirty: bool) -> Result<bool, JournalError> {
        assert!(
            from.is_multiple_of(BLOCK_U64),
            "zeroing starts on a block boundary"
        );
        if from >= to {
            return Ok(false);
        }
        let mut wrote = false;
        let mut block_at = from;
        while block_at < to {
            let stop = (block_at + SCAN_WINDOW).min(align_up(to, BLOCK_U64));
            let mut buf = self
                .file
                .buffer(usize_len((stop - block_at) / BLOCK_U64)?)?;
            read_blocks_tolerant(&self.file, block_at / BLOCK_U64, buf.as_mut_slice()).await?;
            let bytes = buf.as_mut_slice();
            if !(only_dirty && bytes.iter().all(|b| *b == 0)) {
                bytes.fill(0);
                self.file.write_blocks(block_at / BLOCK_U64, bytes).await?;
                wrote = true;
            }
            block_at = stop;
        }
        Ok(wrote)
    }

    /// Mark the last kept entry's slot as cut, unless it already is: the
    /// entries after it are being discarded, so whatever follows it in its
    /// block must never pass for a continuation of its batch — not even
    /// once the reserved slot after it is damaged. Returns the index (within
    /// the segment) whose slot needs rewriting, if any.
    fn mark_cut(&mut self) -> Option<u64> {
        let last = self.recs.last_mut()?;
        if last.slot.cut {
            return None;
        }
        last.slot.cut = true;
        Some(u64_len(self.recs.len()) - 1)
    }

    /// Where the next append batch starts: the first block boundary at or
    /// after the last kept entry.
    fn next_batch_at(&self) -> u64 {
        align_up(self.data_end, BLOCK_U64)
    }

    /// The next index this segment would hold.
    pub fn next_index(&self) -> u64 {
        self.first + u64_len(self.recs.len())
    }

    /// Add this segment's headers and live entries to `atlas`, its file
    /// being `path`.
    pub fn chart(&self, path: &str, atlas: &mut crate::JournalAtlas) {
        atlas.push_headers(path, self.first);
        for (rel, rec) in (0u64..).zip(&self.recs) {
            atlas.push_entry(
                path,
                rec.slot.id(),
                rel,
                (u64::from(rec.slot.offset), rec.slot.length),
                rec.slot.batch_start,
            );
        }
    }

    /// The last record this segment holds.
    pub fn last(&self) -> Option<Rec> {
        self.recs.last().copied()
    }

    /// The records of this segment's last append batch: from the last
    /// record flagged as a batch start to the end. Every segment's first
    /// record starts a batch; should none be flagged, only the last record
    /// is returned — calling acknowledged damage ambiguous could lose it to
    /// a truncation, while the reverse only raises a false alarm.
    pub fn last_batch(&self) -> &[Rec] {
        let from = self
            .recs
            .iter()
            .rposition(|rec| rec.slot.batch_start)
            .unwrap_or_else(|| self.recs.len().saturating_sub(1));
        &self.recs[from..]
    }

    /// How many of `sizes` (on-disk entry sizes) still fit in this segment:
    /// it rolls over when its slot table or its data region fills.
    pub fn fitting(&self, sizes: &[u64]) -> usize {
        let free_slots = u64::from(self.geometry.slot_count) - u64_len(self.recs.len());
        let room = self.geometry.segment_size - self.next_batch_at();
        let mut used = 0;
        let mut count = 0;
        for size in sizes {
            if u64_len(count) >= free_slots || used + size > room {
                break;
            }
            used += size;
            count += 1;
        }
        count
    }

    /// Append one batch at [`next_index`](Self::next_index): `pwrite` the
    /// entries contiguously into the data region from a fresh block,
    /// `pwrite` their slots in one call, `fdatasync` once. Nothing orders the
    /// two writes; recovery tells a torn batch from corruption instead.
    ///
    /// The data write starts on the block boundary after the last entry and
    /// pads its last block with zeros, so it never rewrites a sector an
    /// earlier sync made durable: up to one block of padding per batch, and
    /// no crash during the append can reach an acknowledged entry. The slot
    /// write does rewrite the acknowledged slots that share its blocks; a
    /// crash that loses one leaves its entry intact, and recovery rebuilds
    /// the slot from it.
    pub async fn append(&mut self, batch: &[Record<'_>]) -> Result<(), JournalError> {
        let base = self.next_batch_at();
        let total: u64 = batch
            .iter()
            .map(|record| entry_size(u32::try_from(record.payload.len()).expect("checked")))
            .sum();
        let blocks = usize_len(align_up(total, BLOCK_U64) / BLOCK_U64)?;
        let mut data = self.file.buffer(blocks)?;
        let first_rel = self.recs.len();
        let mut at = 0;
        for (at_batch, record) in batch.iter().enumerate() {
            let batch_start = at_batch == 0;
            let length = u32::try_from(record.payload.len()).expect("checked");
            let size = usize_len(entry_size(length))?;
            let index = self.next_index();
            let offset = base + u64_len(at);
            let entry_crc = encode_entry(
                index,
                record.epoch,
                &record.tag,
                batch_start,
                record.payload,
                &mut data.as_mut_slice()[at..at + size],
            );
            self.recs.push(Rec {
                slot: Slot {
                    index,
                    epoch: record.epoch,
                    offset: u32::try_from(offset).expect("segment fits 32-bit offsets"),
                    length,
                    entry_crc,
                    tag: record.tag,
                    batch_start,
                    cut: false,
                },
                corrupt: false,
            });
            at += size;
        }
        self.file
            .write_blocks(base / BLOCK_U64, data.as_slice())
            .await?;
        let rels: Vec<u64> = (u64_len(first_rel)..u64_len(self.recs.len())).collect();
        self.write_slots(&rels).await?;
        self.file.sync_data().await?;
        self.data_end = base + total;
        Ok(())
    }

    /// Read and verify the entry at `index`, which this segment holds: its
    /// CRC, that its index and epoch match the slot, and that its CRC equals
    /// the slot's `entry_crc`. The slot comes from memory — the startup scan
    /// already verified it.
    pub async fn read(&self, index: u64) -> Result<Entry, JournalError> {
        let rec = self.rec(index);
        if rec.corrupt {
            return Err(JournalError::Corrupt(rec.slot.id()));
        }
        let len = ENTRY_HEADER_SIZE + rec.slot.length as usize;
        let (bytes, failed) = read_bytes(&self.file, u64::from(rec.slot.offset), len).await?;
        verify(&rec, &bytes, failed).map_err(JournalError::Corrupt)
    }

    /// Read and verify every entry in `range` (all held by this segment),
    /// through contiguous reads of up to [`SCAN_WINDOW`] bytes: entries are
    /// packed back to back, so the span from the first to the last is one
    /// sequential read instead of one per entry. Each entry comes back
    /// intact or as the identity of a corrupt one.
    pub async fn read_range(
        &self,
        range: Range<u64>,
        out: &mut Vec<Result<Entry, EntryId>>,
    ) -> Result<(), JournalError> {
        let mut index = range.start;
        while index < range.end {
            // One window: entries from `index` while their span stays within
            // SCAN_WINDOW (always at least one entry, however large).
            let start = u64::from(self.rec(index).slot.offset);
            let mut stop = index + 1;
            let mut end = self.rec(index).end();
            while stop < range.end {
                let next = self.rec(stop);
                if u64::from(next.slot.offset) < start || next.end() - start > SCAN_WINDOW {
                    break;
                }
                end = end.max(next.end());
                stop += 1;
            }
            let base = align_down(start, BLOCK_U64);
            let limit = align_up(end, BLOCK_U64);
            let mut buf = self.file.buffer(usize_len((limit - base) / BLOCK_U64)?)?;
            let failed =
                read_blocks_tolerant(&self.file, base / BLOCK_U64, buf.as_mut_slice()).await?;
            let bytes = buf.as_slice();
            for at in index..stop {
                let rec = self.rec(at);
                if rec.corrupt {
                    out.push(Err(rec.slot.id()));
                    continue;
                }
                let from = usize_len(u64::from(rec.slot.offset) - base)?;
                let to = from + ENTRY_HEADER_SIZE + rec.slot.length as usize;
                let damaged = failed[from / BLOCK..to.div_ceil(BLOCK)].contains(&true);
                out.push(verify(&rec, &bytes[from..to], damaged));
            }
            index = stop;
        }
        Ok(())
    }

    /// The in-memory record of `index`, which this segment holds.
    fn rec(&self, index: u64) -> Rec {
        self.recs[usize::try_from(index - self.first).expect("index within the segment")]
    }

    /// The identity recorded for `index`, which this segment holds.
    pub fn id(&self, index: u64) -> Option<EntryId> {
        let rel = usize::try_from(index.checked_sub(self.first)?).ok()?;
        self.recs.get(rel).map(|rec| rec.slot.id())
    }

    /// Discard `index` and everything after it in this segment, cleaning up
    /// before the next append: reset the discarded slots to their reserved
    /// records and sync, then zero the discarded entries' whole blocks and
    /// sync.
    ///
    /// The slots go first so that a crash part-way leaves a reserved slot
    /// past the cut (the end of the log) — never an identifier beside a
    /// zeroed entry, which would read as corruption. The block holding the
    /// cut is not rewritten: it holds kept entries, and the discarded bytes
    /// in it are inert behind their reserved slots. The last kept slot is
    /// marked cut, with the zeroing, once the resets are durable. The next
    /// batch starts on the block after it.
    pub async fn truncate_from(&mut self, index: u64) -> Result<(), JournalError> {
        let rel = usize_len(index - self.first)?;
        if rel >= self.recs.len() {
            return Ok(());
        }
        let from = u64::from(self.recs[rel].slot.offset);
        let old_end = self.data_end;
        let old_len = u64_len(self.recs.len());
        self.recs.truncate(rel);
        let rels: Vec<u64> = (u64_len(rel)..old_len).collect();
        self.write_slots(&rels).await?;
        self.file.sync_data().await?;
        // Only now the cut mark, with the zeroed blocks: written together
        // with the resets, a torn write could make the mark durable while
        // the identifier after it is neither reset nor intact, and the walk
        // would refuse the continuation an unfinished cut leaves.
        self.data_end = from;
        if let Some(rel) = self.mark_cut() {
            self.write_slots(&[rel]).await?;
        }
        self.zero_data(self.next_batch_at(), old_end, false).await?;
        self.file.sync_data().await?;
        Ok(())
    }
}

/// Format the slots of a segment starting at `first` that fall in `buf`,
/// the bytes of whole blocks from block `at_block`, with their reserved
/// records. Returns whether any did.
fn format_slots(first: u64, geometry: Geometry, at_block: u64, buf: &mut [u8]) -> bool {
    let start = at_block * BLOCK_U64;
    let end = start + u64_len(buf.len());
    let table = SLOT_TABLE_OFFSET..geometry.slot_table_end();
    if end <= table.start || start >= table.end {
        return false;
    }
    let from = table.start.max(start);
    let to = table.end.min(end);
    for offset in (from..to).step_by(SLOT_SIZE) {
        let rel = (offset - SLOT_TABLE_OFFSET) / SLOT_SIZE as u64;
        let at = usize_len(offset - start).expect("within the buffer");
        Slot::encode_reserved(first + rel, &mut buf[at..at + SLOT_SIZE]);
    }
    true
}

/// Check `bytes` — the entry `rec` locates, header and payload, `damaged`
/// when the medium failed to read part of it — against its slot: its CRC,
/// and that its index, epoch, length, CRC and tag agree with the slot.
fn verify(rec: &Rec, bytes: &[u8], damaged: bool) -> Result<Entry, EntryId> {
    let slot = rec.slot;
    let intact = !damaged
        && EntryHeader::parse(bytes).is_some_and(|header| {
            header.index == slot.index
                && header.epoch == slot.epoch
                && header.length == slot.length
                && header.crc == slot.entry_crc
                && header.tag == slot.tag
                && header.batch_start == slot.batch_start
                && entry_crc_ok(&header, bytes)
        });
    if !intact {
        return Err(slot.id());
    }
    Ok(Entry {
        index: slot.index,
        epoch: slot.epoch,
        tag: slot.tag,
        payload: bytes[ENTRY_HEADER_SIZE..ENTRY_HEADER_SIZE + slot.length as usize].to_vec(),
    })
}

/// Find entry `index` when its slot is unusable, from `prev_end`, the end of
/// entry `index − 1` (or the data start, for a segment's first entry).
///
/// Within a batch, entries are packed back to back, so a continuation sits
/// at `prev_end`; a new batch starts on the next block boundary. An entry
/// is accepted only where its batch-start flag says it may be: at
/// `prev_end` as a continuation, or at the boundary as a batch start — when
/// `prev_end` is itself a boundary, either. The boundary is tried first: an
/// entry there with this index can only have been written after any bytes
/// at `prev_end` (which then belong to a discarded suffix). Without
/// `may_continue` — entry `index − 1`'s slot records a cut after it — the
/// bytes at `prev_end` are known to be discarded, and only the boundary
/// counts.
async fn locate<F: StorageFile>(
    window: &mut Window<'_, F>,
    geometry: Geometry,
    prev_end: u64,
    may_continue: bool,
    index: u64,
) -> Result<Option<(u64, EntryHeader)>, JournalError> {
    let boundary = align_up(prev_end, BLOCK_U64);
    if let Some(header) = probe(window, geometry, boundary, index, None).await?
        && (header.batch_start || boundary == prev_end)
    {
        return Ok(Some((boundary, header)));
    }
    if boundary == prev_end || !may_continue {
        return Ok(None);
    }
    Ok(probe(window, geometry, prev_end, index, None)
        .await?
        .filter(|header| !header.batch_start)
        .map(|header| (prev_end, header)))
}

/// Whether an intact entry for `index` sits at `offset`: its magic, its CRC,
/// and its index (the monotonic-index check that catches a misdirected write
/// whose CRC still passes). When the slot is valid, the entry must also
/// agree with it on epoch, length, and CRC — a stale record left by a lost
/// write can pass its own CRC but not this.
async fn probe<F: StorageFile>(
    window: &mut Window<'_, F>,
    geometry: Geometry,
    offset: u64,
    index: u64,
    expected: Option<Slot>,
) -> Result<Option<EntryHeader>, JournalError> {
    if offset < geometry.data_start || !offset.is_multiple_of(8) {
        return Ok(None);
    }
    let Some(head) = window.get(offset, ENTRY_HEADER_SIZE).await? else {
        return Ok(None);
    };
    let Some(header) = EntryHeader::parse(head) else {
        return Ok(None);
    };
    if header.index != index || offset + entry_size(header.length) > geometry.segment_size {
        return Ok(None);
    }
    // Every batch starts on a fresh block: a batch start anywhere else is a
    // stale or misdirected record.
    if header.batch_start && !offset.is_multiple_of(BLOCK_U64) {
        return Ok(None);
    }
    if let Some(slot) = expected
        && (slot.epoch != header.epoch
            || slot.length != header.length
            || slot.entry_crc != header.crc
            || slot.tag != header.tag
            || slot.batch_start != header.batch_start)
    {
        return Ok(None);
    }
    let Some(bytes) = window
        .get(offset, ENTRY_HEADER_SIZE + header.length as usize)
        .await?
    else {
        return Ok(None);
    };
    Ok(entry_crc_ok(&header, bytes).then_some(header))
}
