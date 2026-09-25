//! One segment file: a preallocated, zero-filled [`BlockFile`] holding a
//! header, a slot table, and a data region of contiguous entries.
//!
//! In memory a segment keeps one [`Rec`] per index it holds — the slot the
//! startup scan verified or rebuilt — so a read computes where its entry is
//! and never reads the slot table again. It also keeps the bytes of the
//! partially filled last data block, so an append rewrites them unchanged
//! instead of reading them back.

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

const PREFIX: &str = "seg-";
const SUFFIX: &str = ".wal";

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

/// The file name of the segment whose first index is `first`:
/// `seg-00000000001048576.wal`.
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
    pub corrupt: Vec<(u64, u64)>,
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
    /// First byte past the last kept entry.
    data_end: u64,
    /// The bytes of the block holding `data_end`, up to `data_end`.
    tail: Vec<u8>,
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
    /// copies written, synced, and only then published under its name — so a
    /// segment file either does not exist or is whole.
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
        provider.rename(&temporary, &path).await?;
        provider.sync_dir(dir).await?;
        let file = Self::open_file(provider, &path, direct_io).await?;
        Ok(Self {
            first,
            file,
            geometry,
            recs: Vec::new(),
            data_end: geometry.data_start,
            tail: Vec::new(),
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
    ///
    /// `next_first` is the first index of the following segment, when one
    /// exists: the walk stops there. Wherever the log turns out to end — in
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
        let mut segment = Self {
            first,
            file,
            geometry,
            recs: Vec::new(),
            data_end: geometry.data_start,
            tail: Vec::new(),
        };
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
        } = decide(first, &found)?;
        segment.recs = recs;
        segment.data_end = segment.recs.last().map_or(geometry.data_start, |rec| {
            u64::from(rec.slot.offset) + entry_size(rec.slot.length)
        });
        report.corrupt = corrupt;
        report.slots_rewritten = rewrite_slots.len();
        report.ended = ended || next_first.is_none();

        // Rewrite the lost slots of intact entries.
        let mut dirty: Vec<u64> = rewrite_slots.iter().map(|index| index - first).collect();
        if report.ended {
            // Clean up after the truncation, slots first: every identifier
            // past the end is zeroed and synced before any entry bytes are,
            // so a crash in between can never leave an identifier beside a
            // zeroed entry.
            let kept = u64_len(segment.recs.len());
            let stale: Vec<u64> = (kept..u64::from(geometry.slot_count))
                .filter(|rel| {
                    let at = usize_len(*rel).expect("slot fits") * SLOT_SIZE;
                    table[at..at + SLOT_SIZE].iter().any(|b| *b != 0)
                })
                .collect();
            report.torn |= !stale.is_empty();
            dirty.extend(stale);
        }
        if !dirty.is_empty() || report.header_repaired {
            segment.write_slots(&dirty).await?;
            segment.file.sync_data().await?;
        }
        segment.refresh_tail().await?;
        if report.ended {
            // The entry bytes past the end: if the walk stopped on anything
            // but zeros, whatever the crash left there is zeroed too, so a
            // stale entry that still passes its CRC can never be picked up
            // through a lost slot later.
            let (terminator, _) = read_bytes(
                &segment.file,
                segment.data_end,
                ENTRY_HEADER_SIZE.min(usize_len(geometry.segment_size - segment.data_end)?),
            )
            .await?;
            if report.torn || terminator.iter().any(|b| *b != 0) {
                let wiped = segment
                    .zero_data(segment.data_end, geometry.segment_size, true)
                    .await?;
                if wiped {
                    segment.file.sync_data().await?;
                }
                report.torn |= wiped;
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
    /// when that slot is unusable — at the end of entry *i − 1*. The walk
    /// stops at `next_first`, at the first index with neither an intact
    /// entry nor an identifier (the end of the log), or at the first double
    /// fault. Returns what it found and the raw slot table.
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
        let limit = next_first.map_or(u64::from(geometry.slot_count), |next| next - self.first);

        let mut window = Window::new(&self.file, geometry.segment_size);
        let mut found = Vec::new();
        // The first entry of a segment starts the data region.
        let mut prev_end = geometry.data_start;
        for rel in 0..limit {
            let at = usize_len(rel)? * SLOT_SIZE;
            let index = self.first + rel;
            // An identifier the medium cannot read is damaged, not absent:
            // zeros there would pass for "no identifier".
            let slot = if failed[at / BLOCK] {
                SlotState::Bad
            } else {
                Slot::decode(&table[at..at + SLOT_SIZE], index)
            };
            let (offset, expected) = match slot {
                SlotState::Valid(slot) => (u64::from(slot.offset), Some(slot)),
                _ => (prev_end, None),
            };
            let entry = probe(&mut window, geometry, offset, index, expected)
                .await?
                .map(|header| (offset, header));
            prev_end = match (entry, slot) {
                (Some((offset, header)), _) => offset + entry_size(header.length),
                (None, SlotState::Valid(slot)) => u64::from(slot.offset) + entry_size(slot.length),
                (None, _) => prev_end,
            };
            found.push(Found { slot, entry });
            // Neither an intact entry nor an identifier: the end of the log
            // (empty slot) or a double fault (damaged slot). Either way the
            // walk has no way to go on.
            if entry.is_none() && !matches!(slot, SlotState::Valid(_)) {
                break;
            }
        }
        Ok((found, table))
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
            let mut buf = self.file.buffer(run_end - run_start)?;
            for (at, bytes) in buf.as_mut_slice().chunks_exact_mut(SLOT_SIZE).enumerate() {
                let rel = usize_len(first_block * per_block)? + at;
                if let Some(rec) = self.recs.get(rel) {
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
    /// `from` in its block. With `only_dirty`, windows already zero past
    /// `from` are left untouched. Returns whether anything was written. Does
    /// not sync.
    async fn zero_data(&self, from: u64, to: u64, only_dirty: bool) -> Result<bool, JournalError> {
        if from >= to {
            return Ok(false);
        }
        let mut wrote = false;
        let mut block_at = align_down(from, BLOCK_U64);
        while block_at < to {
            let stop = (block_at + SCAN_WINDOW).min(align_up(to, BLOCK_U64));
            let mut buf = self
                .file
                .buffer(usize_len((stop - block_at) / BLOCK_U64)?)?;
            read_blocks_tolerant(&self.file, block_at / BLOCK_U64, buf.as_mut_slice()).await?;
            let keep = usize_len(from.saturating_sub(block_at))?;
            let bytes = buf.as_mut_slice();
            if !(only_dirty && bytes[keep..].iter().all(|b| *b == 0)) {
                bytes[keep..].fill(0);
                self.file.write_blocks(block_at / BLOCK_U64, bytes).await?;
                wrote = true;
            }
            block_at = stop;
        }
        Ok(wrote)
    }

    /// Reload the cached bytes of the block holding `data_end`.
    async fn refresh_tail(&mut self) -> Result<(), JournalError> {
        let start = align_down(self.data_end, BLOCK_U64);
        self.tail = read_bytes(&self.file, start, usize_len(self.data_end - start)?)
            .await?
            .0;
        Ok(())
    }

    /// The next index this segment would hold.
    pub fn next_index(&self) -> u64 {
        self.first + u64_len(self.recs.len())
    }

    /// The last record this segment holds.
    pub fn last(&self) -> Option<Rec> {
        self.recs.last().copied()
    }

    /// How many of `sizes` (on-disk entry sizes) still fit in this segment:
    /// it rolls over when its slot table or its data region fills.
    pub fn fitting(&self, sizes: &[u64]) -> usize {
        let free_slots = u64::from(self.geometry.slot_count) - u64_len(self.recs.len());
        let room = self.geometry.segment_size - self.data_end;
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
    /// entries contiguously into the data region, `pwrite` their slots in
    /// one call, `fdatasync` once. Nothing orders the two writes; recovery
    /// tells a torn batch from corruption instead.
    pub async fn append(&mut self, batch: &[(u64, &[u8])]) -> Result<(), JournalError> {
        let start = self.data_end;
        let base = align_down(start, BLOCK_U64);
        let total: u64 = batch
            .iter()
            .map(|(_, payload)| entry_size(u32::try_from(payload.len()).expect("checked")))
            .sum();
        let blocks = usize_len((align_up(start + total, BLOCK_U64) - base) / BLOCK_U64)?;
        let mut data = self.file.buffer(blocks)?;
        // The block the batch starts in may already hold entries: they are
        // rewritten byte for byte from the cached copy.
        let lead = self.tail.len();
        data.as_mut_slice()[..lead].copy_from_slice(&self.tail);
        let first_rel = self.recs.len();
        let mut at = lead;
        for (epoch, payload) in batch {
            let length = u32::try_from(payload.len()).expect("checked");
            let size = usize_len(entry_size(length))?;
            let index = self.next_index();
            let offset = base + u64_len(at);
            let entry_crc = encode_entry(
                index,
                *epoch,
                payload,
                &mut data.as_mut_slice()[at..at + size],
            );
            self.recs.push(Rec {
                slot: Slot {
                    index,
                    epoch: *epoch,
                    offset: u32::try_from(offset).expect("segment fits 32-bit offsets"),
                    length,
                    entry_crc,
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
        self.data_end = start + total;
        let tail_start = usize_len(align_down(self.data_end, BLOCK_U64) - base)?;
        self.tail = data.as_slice()[tail_start..at].to_vec();
        Ok(())
    }

    /// Read and verify the entry at `index`, which this segment holds: its
    /// CRC, that its index and epoch match the slot, and that its CRC equals
    /// the slot's `entry_crc`. The slot comes from memory — the startup scan
    /// already verified it.
    pub async fn read(&self, index: u64) -> Result<Entry, JournalError> {
        let rec = self.recs[usize_len(index - self.first)?];
        let corrupt = JournalError::Corrupt {
            index,
            epoch: rec.slot.epoch,
        };
        if rec.corrupt {
            return Err(corrupt);
        }
        let len = ENTRY_HEADER_SIZE + rec.slot.length as usize;
        let (bytes, failed) = read_bytes(&self.file, u64::from(rec.slot.offset), len).await?;
        let intact = !failed
            && EntryHeader::parse(&bytes).is_some_and(|header| {
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
        self.recs.get(rel).map(|rec| rec.slot.epoch)
    }

    /// Discard `index` and everything after it in this segment, cleaning up
    /// before the next append: zero the discarded slots and sync, then zero
    /// the discarded entries and sync.
    ///
    /// The slots go first so that a crash part-way can only leave intact
    /// entries without identifiers (kept, a prefix of the old log) or zeroed
    /// entries without identifiers (the end of the log) — never an
    /// identifier beside a zeroed entry, which would read as corruption.
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
        self.zero_data(from, old_end, false).await?;
        self.file.sync_data().await?;
        self.data_end = from;
        self.refresh_tail().await
    }
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
