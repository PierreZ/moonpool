//! One segment file: a preallocated, zero-filled [`BlockFile`] holding a
//! header, a slot table, and a data region.
//!
//! In memory a segment keeps one [`Rec`] per index it holds — the slot the
//! startup scan verified or rebuilt — so a read computes where its entry is
//! and never reads the slot table again.

use std::io;

use moonpool_core::{BlockFile, DirectIo, OpenOptions, StorageFile, StorageProvider};

use crate::JournalError;
use crate::layout::{
    BLOCK, BLOCK_U64, ENTRY_HEADER_SIZE, EntryHeader, Geometry, Header, SLOT_SIZE,
    SLOT_TABLE_OFFSET, Slot, SlotState, align_down, align_up, encode_entry, entry_crc_ok,
    entry_size,
};
use crate::scan::{Decision, Found, Rec, decide};

/// Bytes read at a time while the recovery scan walks the data region.
const SCAN_WINDOW: u64 = 1 << 20;

/// Bytes zeroed per write while preallocating a segment.
const FILL_CHUNK: u64 = 1 << 20;

/// An entry read back from the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its log index.
    pub index: u64,
    /// The epoch (term) it was written with.
    pub epoch: u64,
    /// The caller's bytes.
    pub payload: Vec<u8>,
}

/// The name of the segment whose first index is `first`.
pub(crate) fn segment_path(dir: &str, first: u64) -> String {
    format!("{dir}/seg-{first:020}.wal")
}

/// What the recovery of one segment found.
#[derive(Debug, Default)]
pub(crate) struct SegmentRecovery {
    pub corrupt: Vec<(u64, u64)>,
    pub ambiguous_tail: Vec<(u64, u64)>,
    pub slots_rewritten: usize,
    pub torn: bool,
    pub header_repaired: bool,
}

/// Where a segment's log ends, as far as recovery is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bound {
    /// A later segment starts at this index: every index before it was
    /// synced before the rollover, so this segment has no tail.
    Sealed(u64),
    /// The last segment: it ends wherever its last batch does.
    Tail,
    /// The last segment, with a suffix truncation at this index in progress
    /// when the journal stopped: everything from the fence on is discarded,
    /// in whatever state the interrupted zeroing left it.
    Fenced(u64),
}

impl Bound {
    /// The first index this segment does not hold, when known up front.
    fn end(self) -> Option<u64> {
        match self {
            Self::Sealed(end) | Self::Fenced(end) => Some(end),
            Self::Tail => None,
        }
    }
}

/// Where the segments of one journal live and how recovery treats them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Placement<'a> {
    pub dir: &'a str,
    pub geometry: Geometry,
    pub direct_io: DirectIo,
    /// The journal's live start: indexes below it are not judged.
    pub start: u64,
    /// How far past the last kept entry an unacknowledged write can reach.
    pub max_batch: u64,
}

pub(crate) struct Segment<F> {
    pub first: u64,
    file: BlockFile<F>,
    geometry: Geometry,
    recs: Vec<Option<Rec>>,
    /// First byte past the last kept entry.
    data_end: u64,
}

fn u64_len(len: usize) -> u64 {
    u64::try_from(len).expect("usize fits u64")
}

fn usize_len(len: u64) -> io::Result<usize> {
    usize::try_from(len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "length overflows usize"))
}

/// Read `len` bytes at `offset` through whole blocks.
async fn read_bytes<F: StorageFile>(
    file: &BlockFile<F>,
    offset: u64,
    len: usize,
) -> io::Result<Vec<u8>> {
    let start = align_down(offset, BLOCK_U64);
    let end = align_up(offset + u64_len(len), BLOCK_U64);
    let mut buf = file.buffer(usize_len((end - start) / BLOCK_U64)?)?;
    file.read_blocks(start / BLOCK_U64, buf.as_mut_slice())
        .await?;
    let skip = usize_len(offset - start)?;
    Ok(buf.as_slice()[skip..skip + len].to_vec())
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
            self.bytes = read_bytes(self.file, start, usize_len(stop - start)?).await?;
            self.start = start;
        }
        let skip = usize_len(offset - self.start)?;
        Ok(Some(&self.bytes[skip..skip + len]))
    }
}

impl<F: StorageFile> Segment<F> {
    /// Create a fresh segment: zero-filled to its full size, both header
    /// copies written, synced, and its name made durable.
    pub async fn create<P: StorageProvider<File = F>>(
        provider: &P,
        dir: &str,
        first: u64,
        geometry: Geometry,
        direct_io: DirectIo,
    ) -> Result<Self, JournalError> {
        let path = segment_path(dir, first);
        // A leftover from a creation or deletion a crash interrupted: never
        // named by the manifest, so nothing in it is live.
        if provider.exists(&path).await? {
            provider.delete(&path).await?;
        }
        let file = provider
            .open(&path, OpenOptions::create_new_write().read(true))
            .await?;
        let blocks = BlockFile::new(file, BLOCK)?;
        let chunk_blocks = FILL_CHUNK / BLOCK_U64;
        let total_blocks = geometry.segment_size / BLOCK_U64;
        let mut zeros = blocks.buffer(usize_len(chunk_blocks)?)?;
        let mut block = 0;
        while block < total_blocks {
            let count = chunk_blocks.min(total_blocks - block);
            let len = usize_len(count * BLOCK_U64)?;
            let buf = &mut zeros.as_mut_slice()[..len];
            if block == 0 {
                let header = Header {
                    first_index: first,
                    geometry,
                };
                header.encode(&mut buf[..BLOCK]);
                header.encode(&mut buf[BLOCK..2 * BLOCK]);
            }
            blocks.write_blocks(block, buf).await?;
            if block == 0 {
                buf[..2 * BLOCK].fill(0);
            }
            block += count;
        }
        // Length and bytes alike: after this, fdatasync never has metadata
        // to write, because the file never changes size again.
        blocks.sync().await?;
        drop(blocks);
        provider.sync_dir(dir).await?;
        let file = Self::open_file(provider, &path, direct_io).await?;
        Ok(Self {
            first,
            file,
            geometry,
            recs: Vec::new(),
            data_end: geometry.data_start,
        })
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
    pub async fn recover<P: StorageProvider<File = F>>(
        provider: &P,
        placement: &Placement<'_>,
        first: u64,
        bound: Bound,
    ) -> Result<(Self, SegmentRecovery), JournalError> {
        let Placement {
            dir,
            geometry,
            direct_io,
            start,
            max_batch,
        } = *placement;
        let path = segment_path(dir, first);
        if !provider.exists(&path).await? {
            return Err(JournalError::SegmentMissing { first_index: first });
        }
        let file = Self::open_file(provider, &path, direct_io).await?;
        let actual = file.get_ref().size().await?;
        if actual != geometry.segment_size {
            return Err(JournalError::SegmentSize {
                first_index: first,
                expected: geometry.segment_size,
                actual,
            });
        }
        let mut segment = Self {
            first,
            file,
            geometry,
            recs: Vec::new(),
            data_end: geometry.data_start,
        };
        let mut report = SegmentRecovery {
            header_repaired: segment.check_header().await?,
            ..SegmentRecovery::default()
        };

        let (found, table) = segment.walk(bound, max_batch).await?;
        let Decision {
            recs,
            rewrite_slots,
            corrupt,
            ambiguous_tail,
            torn,
        } = decide(first, start, &found, matches!(bound, Bound::Sealed(_)))?;
        segment.recs = recs;
        segment.data_end = segment
            .recs
            .iter()
            .flatten()
            .map(|rec| u64::from(rec.slot.offset) + entry_size(rec.slot.length))
            .max()
            .unwrap_or(geometry.data_start);

        // Slots to rewrite: every lost slot of a kept entry, and every
        // non-empty slot past the kept range.
        let kept = u64_len(segment.recs.len());
        let mut dirty: Vec<u64> = rewrite_slots.iter().map(|index| index - first).collect();
        dirty.extend((kept..geometry.slot_count.into()).filter(|rel| {
            let at = usize_len(*rel).expect("slot fits") * SLOT_SIZE;
            table[at..at + SLOT_SIZE].iter().any(|b| *b != 0)
        }));
        segment.write_slots(&dirty).await?;

        // Whatever the crashed incarnation wrote past the kept entries must
        // not survive: a stale entry that still passes its CRC would otherwise
        // be picked up the next time a slot is lost.
        let wipe = match bound {
            Bound::Sealed(_) => None,
            Bound::Tail => {
                // One unsynced batch reaches at most `max_batch` past the
                // last synced one. Start at the next block boundary: the
                // block holding the last kept entry is never rewritten, so
                // recovery cannot tear an acknowledged entry. What is left
                // between the entry's end and that boundary is reachable only
                // as the fallback candidate for the index just cut, which
                // failed its check here.
                let reach = ambiguous_tail
                    .iter()
                    .filter_map(|(index, _)| {
                        match found[usize_len(index - first).expect("fits")].slot {
                            SlotState::Valid(slot) => {
                                Some(u64::from(slot.offset) + entry_size(slot.length))
                            }
                            _ => None,
                        }
                    })
                    .fold(segment.data_end, u64::max);
                let hi =
                    align_up(reach.saturating_add(max_batch), BLOCK_U64).min(geometry.segment_size);
                Some((align_up(segment.data_end, BLOCK_U64), hi))
            }
            // An interrupted truncation may have left intact, CRC-valid
            // entries anywhere up to the old end — including the one right
            // after the cut, in the same block — so everything goes.
            Bound::Fenced(_) => Some((segment.data_end, geometry.segment_size)),
        };
        let mut wiped_data = false;
        if let Some((from, to)) = wipe {
            wiped_data = segment.zero_data(from, to, true).await?;
        }
        if !dirty.is_empty() || wiped_data || report.header_repaired {
            segment.file.sync_data().await?;
        }

        report.corrupt = corrupt;
        report.ambiguous_tail = ambiguous_tail;
        report.slots_rewritten = rewrite_slots.len();
        report.torn = torn || wiped_data;
        Ok((segment, report))
    }

    /// Verify the two header copies, repairing one from the other.
    /// Returns whether a copy was rewritten.
    async fn check_header(&self) -> Result<bool, JournalError> {
        let mut buf = self.file.buffer(2)?;
        self.file.read_blocks(0, buf.as_mut_slice()).await?;
        let expected = Header {
            first_index: self.first,
            geometry: self.geometry,
        };
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

    /// Walk the indexes in order, locating each entry through its slot or,
    /// when the slot is unusable, through the end of the entry before it.
    /// Returns what was found and the raw slot table.
    ///
    /// When an entry and its slot are both unusable the chain loses its
    /// place, and a later entry whose slot shares the damaged sector would be
    /// invisible — so a damaged mid-log index would pass for a torn tail. The
    /// walk therefore resynchronises: see [`resync`](Self::resync).
    async fn walk(
        &self,
        bound: Bound,
        max_batch: u64,
    ) -> Result<(Vec<Found>, Vec<u8>), JournalError> {
        let geometry = self.geometry;
        let mut table = self.file.buffer(usize_len(geometry.slot_table_blocks())?)?;
        self.file
            .read_blocks(SLOT_TABLE_OFFSET / BLOCK_U64, table.as_mut_slice())
            .await?;
        let table = table.as_slice()
            [..usize_len(u64::from(geometry.slot_count) * SLOT_SIZE as u64)?]
            .to_vec();
        let states: Vec<SlotState> = table
            .chunks_exact(SLOT_SIZE)
            .enumerate()
            .map(|(rel, bytes)| Slot::decode(bytes, self.first + u64_len(rel)))
            .collect();
        let last_nonempty = states.iter().rposition(|state| *state != SlotState::Empty);
        let limit = bound
            .end()
            .map_or(u64::from(geometry.slot_count), |end| end - self.first);
        let sealed = matches!(bound, Bound::Sealed(_));

        let mut window = Window::new(&self.file, geometry.segment_size);
        let mut found = Vec::new();
        // The first entry of a segment starts the data region.
        let mut prev_end = Some(geometry.data_start);
        let mut prev_good = true;
        // The end of the last entry whose position is known, and a later
        // entry a resynchronisation found past a break in the chain.
        let mut known_end = geometry.data_start;
        let mut resynced: Option<(u64, u64)> = None;
        let mut searched_from = None;
        for rel in 0..limit {
            let rel_usize = usize_len(rel)?;
            let index = self.first + rel;
            let past_slots = last_nonempty.is_none_or(|last| rel_usize > last);
            let resync_ahead = resynced.is_some_and(|(target, _)| target >= index);
            if !sealed && past_slots && !prev_good && !resync_ahead {
                break;
            }
            let slot = states[rel_usize];
            if prev_end.is_none()
                && !matches!(slot, SlotState::Valid(_))
                && resynced.is_none_or(|(target, _)| target < index)
                && searched_from != Some(known_end)
            {
                searched_from = Some(known_end);
                resynced = self
                    .resync(&mut window, known_end, index, limit, max_batch)
                    .await?;
            }
            let mut candidates: Vec<u64> = match (slot, prev_end) {
                (SlotState::Valid(slot), _) => vec![u64::from(slot.offset)],
                // Inside a batch the next entry follows directly; a new batch
                // starts on the next block boundary.
                (_, Some(end)) => {
                    let boundary = align_up(end, BLOCK_U64);
                    if boundary == end {
                        vec![end]
                    } else {
                        vec![end, boundary]
                    }
                }
                (_, None) => Vec::new(),
            };
            if let Some((target, offset)) = resynced
                && target == index
            {
                candidates.push(offset);
            }
            let expected = match slot {
                SlotState::Valid(slot) => Some(slot),
                _ => None,
            };
            let mut entry = None;
            for offset in candidates {
                if let Some(header) = probe(&mut window, geometry, offset, index, expected).await? {
                    entry = Some((offset, header));
                    break;
                }
            }
            prev_good = entry.is_some();
            prev_end = match (entry, slot) {
                (Some((offset, header)), _) => Some(offset + entry_size(header.length)),
                (None, SlotState::Valid(slot)) => {
                    Some(u64::from(slot.offset) + entry_size(slot.length))
                }
                (None, _) => None,
            };
            if let Some(end) = prev_end {
                known_end = known_end.max(end);
            }
            found.push(Found { slot, entry });
        }
        Ok((found, table))
    }

    /// Search for the first intact entry past a break in the chain.
    ///
    /// Every batch starts on a block boundary, so an entry that cannot be
    /// reached through a slot or its predecessor is either inside the batch
    /// that broke or starts a later one: probing the block boundaries from
    /// the last known entry end finds the next batch. The search covers one
    /// `max_batch` of bytes — the reach of the batch that broke — and returns
    /// the entry's index (past `index`) and offset.
    async fn resync(
        &self,
        window: &mut Window<'_, F>,
        from: u64,
        index: u64,
        limit: u64,
        max_batch: u64,
    ) -> Result<Option<(u64, u64)>, JournalError> {
        let geometry = self.geometry;
        let stop = from
            .saturating_add(max_batch)
            .saturating_add(BLOCK_U64)
            .min(geometry.segment_size);
        let mut offset = align_up(from, BLOCK_U64);
        while offset < stop {
            let header = window
                .get(offset, ENTRY_HEADER_SIZE)
                .await?
                .and_then(EntryHeader::parse);
            if let Some(header) = header
                && header.index > index
                && header.index < self.first + limit
                && probe(window, geometry, offset, header.index, None)
                    .await?
                    .is_some()
            {
                return Ok(Some((header.index, offset)));
            }
            offset += BLOCK_U64;
        }
        Ok(None)
    }

    /// Rewrite the slot-table blocks holding `rels` from the in-memory
    /// records (zeros past the kept range). Does not sync.
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
            let count = run_end - run_start;
            let mut buf = self.file.buffer(count)?;
            for (at, bytes) in buf.as_mut_slice().chunks_exact_mut(SLOT_SIZE).enumerate() {
                let rel = usize_len(first_block * per_block)? + at;
                if let Some(Some(rec)) = self.recs.get(rel) {
                    rec.slot.encode(bytes);
                }
            }
            self.file
                .write_blocks(SLOT_TABLE_OFFSET / BLOCK_U64 + first_block, buf.as_slice())
                .await?;
            run_start = run_end;
        }
        Ok(())
    }

    /// Zero `[from, to)` of the data region, preserving the bytes before
    /// `from` in its block. With `only_dirty`, blocks already zero past
    /// `from` are left untouched, so a clean shutdown rewrites nothing.
    /// Returns whether anything was written. Does not sync.
    async fn zero_data(&self, from: u64, to: u64, only_dirty: bool) -> Result<bool, JournalError> {
        if from >= to {
            return Ok(false);
        }
        let start = align_down(from, BLOCK_U64);
        let mut wrote = false;
        let mut block_at = start;
        while block_at < to {
            let stop = (block_at + SCAN_WINDOW).min(align_up(to, BLOCK_U64));
            let count = usize_len((stop - block_at) / BLOCK_U64)?;
            let mut buf = self.file.buffer(count)?;
            self.file
                .read_blocks(block_at / BLOCK_U64, buf.as_mut_slice())
                .await?;
            let keep = usize_len(from.saturating_sub(block_at))?;
            let bytes = buf.as_mut_slice();
            if only_dirty && bytes[keep..].iter().all(|b| *b == 0) {
                block_at = stop;
                continue;
            }
            bytes[keep..].fill(0);
            self.file.write_blocks(block_at / BLOCK_U64, bytes).await?;
            wrote = true;
            block_at = stop;
        }
        Ok(wrote)
    }

    /// The next index this segment would hold.
    pub fn next_index(&self) -> u64 {
        self.first + u64_len(self.recs.len())
    }

    /// How many of `sizes` (on-disk entry sizes) fit into this segment as one
    /// batch of at most `max_batch` bytes.
    pub fn fitting(&self, sizes: &[u64], max_batch: u64) -> usize {
        let free_slots = u64::from(self.geometry.slot_count) - u64_len(self.recs.len());
        let start = align_up(self.data_end, BLOCK_U64);
        let room = self
            .geometry
            .segment_size
            .saturating_sub(start)
            .min(max_batch);
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

    /// Append one batch at [`next_index`](Self::next_index): the entries in
    /// one write, their slots in another, then one `fdatasync`. Nothing orders
    /// the two writes; recovery tells a torn batch from corruption instead.
    pub async fn append(&mut self, batch: &[(u64, &[u8])]) -> Result<(), JournalError> {
        let start = align_up(self.data_end, BLOCK_U64);
        let total: u64 = batch
            .iter()
            .map(|(_, payload)| entry_size(u32::try_from(payload.len()).expect("checked")))
            .sum();
        let mut data = self
            .file
            .buffer(usize_len(align_up(total, BLOCK_U64) / BLOCK_U64)?)?;
        let first_rel = self.recs.len();
        let mut at = 0usize;
        for (pos, (epoch, payload)) in batch.iter().enumerate() {
            let batch_pos = u32::try_from(pos).expect("a batch fits one slot table");
            let length = u32::try_from(payload.len()).expect("checked");
            let size = usize_len(entry_size(length))?;
            let index = self.next_index();
            let offset = start + u64_len(at);
            let entry_crc = encode_entry(
                index,
                *epoch,
                batch_pos,
                payload,
                &mut data.as_mut_slice()[at..at + size],
            );
            self.recs.push(Some(Rec {
                slot: Slot {
                    index,
                    epoch: *epoch,
                    offset: u32::try_from(offset).expect("segment fits 32-bit offsets"),
                    length,
                    entry_crc,
                },
                corrupt: false,
            }));
            at += size;
        }
        self.file
            .write_blocks(start / BLOCK_U64, data.as_slice())
            .await?;
        let rels: Vec<u64> = (u64_len(first_rel)..u64_len(self.recs.len())).collect();
        self.write_slots(&rels).await?;
        self.file.sync_data().await?;
        self.data_end = start + total;
        Ok(())
    }

    /// Read and verify the entry at `index`, which this segment holds.
    pub async fn read(&self, index: u64) -> Result<Entry, JournalError> {
        let rel = usize_len(index - self.first)?;
        let Some(rec) = self.recs[rel] else {
            return Err(JournalError::OutOfRange {
                index,
                start: index + 1,
                next: self.next_index(),
            });
        };
        let corrupt = JournalError::Corrupt {
            index,
            epoch: rec.slot.epoch,
        };
        if rec.corrupt {
            return Err(corrupt);
        }
        let len = ENTRY_HEADER_SIZE + rec.slot.length as usize;
        let bytes = read_bytes(&self.file, u64::from(rec.slot.offset), len).await?;
        let intact = EntryHeader::parse(&bytes).is_some_and(|header| {
            header.index == index
                && header.epoch == rec.slot.epoch
                && header.length == rec.slot.length
                && header.crc == rec.slot.entry_crc
                && entry_crc_ok(&header, &bytes)
        });
        if !intact {
            return Err(corrupt);
        }
        Ok(Entry {
            index,
            epoch: rec.slot.epoch,
            payload: bytes[ENTRY_HEADER_SIZE..].to_vec(),
        })
    }

    /// The epoch recorded for `index`, which this segment holds.
    pub fn epoch(&self, index: u64) -> Option<u64> {
        let rel = usize::try_from(index.checked_sub(self.first)?).ok()?;
        self.recs
            .get(rel)
            .copied()
            .flatten()
            .map(|rec| rec.slot.epoch)
    }

    /// Discard `index` and everything after it in this segment: zero the
    /// discarded slots and entries, then sync — so a crash can never leave an
    /// old slot beside a new entry.
    pub async fn truncate_from(&mut self, index: u64) -> Result<(), JournalError> {
        let rel = usize_len(index - self.first)?;
        if rel >= self.recs.len() {
            return Ok(());
        }
        let from =
            self.recs[rel].map_or(self.geometry.data_start, |rec| u64::from(rec.slot.offset));
        let to = align_up(self.data_end, BLOCK_U64);
        let old_len = u64_len(self.recs.len());
        self.recs.truncate(rel);
        let rels: Vec<u64> = (u64_len(rel)..old_len).collect();
        self.write_slots(&rels).await?;
        self.zero_data(from, to, false).await?;
        self.file.sync_data().await?;
        self.data_end = from;
        Ok(())
    }
}

/// Whether an intact entry for `index` sits at `offset`; when the slot is
/// valid, the entry must also agree with it (a stale record left by a lost or
/// misdirected write can pass its own CRC but not this).
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
    if let Some(slot) = expected
        && (slot.epoch != header.epoch
            || slot.length != header.length
            || slot.entry_crc != header.crc)
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
