//! Opening's scan: CLSTORE's detection, disentanglement and identification
//! over the persist log and the entry log of every segment.
//!
//! **Pass 1, structure.** Each segment's persist log is parsed batch by
//! batch. A batch starts on a block; every record names its batch, its index
//! and the batch's record count, so any one valid record locates its whole
//! batch. A batch with no valid record is found through its first entry (an
//! entry carries the same fields). A damaged record is matched with its
//! entry by walking the entry log: entries are packed back to back inside a
//! batch, and each batch's entries start on the block after the last one's.
//! A batch missing while later records exist is [`OpenError::LostBatch`],
//! never an end; the persist log ends at the first block boundary after
//! which nothing valid follows. A record or entry of a batch older than its
//! segment is stale, not valid: a recycled file's old bytes (see
//! [`crate::spare`]) read like never-written ones.
//!
//! **The rule of position.** In one segment, batch numbers grow with the
//! place in the persist log. A valid record at or after the boundary the
//! scan is at, whose batch is not above the last batch found so far, is
//! old bytes too: it cannot prove that a batch is missing. ([`Known::is_stale`]
//! still covers old bytes exactly at the end of the log.)
//!
//! **A record at its own place.** The first record of a batch names the
//! first entry of the batch, and the scan knows where the entries of each
//! boundary start. A first record that names another offset was written to
//! the wrong block (a misdirected write): it reads as damaged, and the
//! batch is located from its first entry.
//!
//! **Retired slots.** A retire record ([`Retire`]) names a slot of an
//! earlier batch whose record may be lost: a slot a crash tore, or a dead
//! slot whose entry is damaged. It carries the offset where the entry walk
//! continues after that slot, so an intact entry behind a retired slot is
//! found again. The retire records come after their slots in the log, so
//! pass 1 runs again with them when it needs them: when a lost slot breaks
//! the walk of the slots after it, or when a batch whose first slot is
//! retired cannot be located ([`pass_one_retired`]).
//!
//! **Pass 2, verdicts** (the table in the crate docs). A lost slot that a
//! retire record names is ignored. Only the **last batch** and damaged
//! records cost entry reads: the rest of the log is checked when it is read.
//!
//! **What the metainfo proves.** Its high-water mark ([`Known::mark`]) is a
//! batch known durable: a slot of a batch at or below it whose record and
//! entry are both damaged is corruption, never a crash. Its open generation
//! ([`Known::generation`]) is above every batch number written so far: a
//! batch of a later generation proves the metainfo older than the log
//! ([`OpenError::MetaRolledBack`]).

use std::collections::BTreeMap;

use moonpool_core::{AlignedBuf, LocalStorageFile};

use crate::format::{
    BLOCK_U64, EntryHeader, FLAG_ORDERED, Geometry, Id, Kind, ParsedEntry, RECORD_SIZE,
    RECORDS_PER_BLOCK, Record, Retire, Slot, align_up, first_batch_of, generation_of, is_zero,
    stamp_is_ahead, u64_of, usize_of,
};
use crate::segment::Segment;
use crate::{OpenError, RecordRepair};

/// Where a live entry is, and what is known of its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Located {
    /// The segment (by its first batch).
    pub segment: u64,
    /// The record's byte offset in the segment file.
    pub record_at: u64,
    pub offset: u32,
    pub len: u32,
    pub entry_crc: u32,
    pub id: Id,
    pub batch: u64,
    pub index: u16,
    pub count: u16,
    pub flags: u8,
    pub damaged: bool,
}

/// One applied operation, in log order.
#[derive(Clone, Debug)]
pub(crate) enum Op {
    Put { position: u64, at: Located },
    Clear { start: u64, end: u64 },
}

/// What a slot of a batch turned out to be after pass 1.
#[derive(Clone, Copy, Debug)]
enum Found {
    /// A valid record that belongs here.
    Record(Record),
    /// The record is damaged, its entry intact: the record rebuilt from it.
    Rebuilt(Record),
    /// The record and the payload of a put are damaged, the entry header
    /// intact: the header names the entry and its place, but proves
    /// nothing of its payload. Never written back as a record.
    Headed(Record),
    /// Both damaged, or the entry cannot be located.
    Lost,
}

impl Found {
    /// The record this slot holds, valid, rebuilt, or read from an entry
    /// header: where its entry is.
    fn record(&self) -> Option<&Record> {
        match self {
            Found::Record(record) | Found::Rebuilt(record) | Found::Headed(record) => Some(record),
            Found::Lost => None,
        }
    }

    /// The record a repair may write back: valid, or rebuilt from an intact
    /// entry. A record read from a header alone is not.
    fn proven(&self) -> Option<&Record> {
        match self {
            Found::Record(record) | Found::Rebuilt(record) => Some(record),
            Found::Headed(_) | Found::Lost => None,
        }
    }
}

/// One persist write of the open or of a scrub: `(segment index, offset,
/// bytes)`.
pub(crate) type BlockWrite = (usize, u64, AlignedBuf);

/// The slots that retire records name, by `(batch, index)`.
pub(crate) type Retires = BTreeMap<(u64, u16), Retire>;

/// What pass 1 knows beyond the segment it reads.
#[derive(Clone, Copy)]
struct Context<'a> {
    tag: u32,
    known: Known,
    retires: &'a Retires,
}

/// Pass 1 over every segment: per segment, its batches and its two
/// cursors.
type PerSegment = Vec<(Vec<BatchScan>, u64, u64)>;

#[derive(Debug)]
struct BatchScan {
    seq: u64,
    count: u16,
    /// Byte offset of its first persist record in the segment file.
    persist_at: u64,
    slots: Vec<Found>,
}

#[derive(Debug, Default)]
pub(crate) struct Scan {
    pub ops: Vec<Op>,
    /// The last batch in the log (0: none).
    pub last_batch: u64,
    /// Per segment: where the next batch's persist records and entries go.
    pub cursors: Vec<(u64, u64)>,
    pub corrupt: Vec<(u64, Id)>,
    pub ambiguous: Vec<(u64, Id)>,
    pub rebuilt: u32,
    /// The slots of the last batch of an open generation that a crash
    /// tore: the open retires them in a new batch before it raises the
    /// high-water mark.
    pub torn: Vec<Retire>,
    /// The slots judged `Corrupt` from their entry header alone, with their
    /// position: the open retires each one that a later batch replaced,
    /// because nothing else ever will.
    pub headed: Vec<(u64, Retire)>,
    /// The persist blocks the open writes again, in order, each durable
    /// before the next (see [`repair_batch`]).
    pub writes: Vec<BlockWrite>,
    /// The blocks of earlier batches written again, and those left.
    pub repair: RecordRepair,
}

/// An entry whose header passed its CRC, and whether its payload did too.
#[derive(Clone, Copy, Debug)]
struct EntryRead {
    entry: ParsedEntry,
    /// The payload passed its CRC: the entry is intact.
    whole: bool,
}

impl EntryRead {
    /// Whether this is, intact, the entry `record` names.
    fn proves(&self, record: &Record) -> bool {
        self.whole && entry_matches(record, &self.entry.header, self.entry.crc)
    }
}

/// The entry at `offset`, if its header is intact and lies inside the entry
/// log.
async fn read_entry<F: LocalStorageFile>(
    segment: &Segment<F>,
    offset: u64,
    tag: u32,
) -> std::io::Result<Option<EntryRead>> {
    let geometry = segment.geometry;
    if offset < geometry.entry_start() || offset + u64_of(RECORD_SIZE) > geometry.entry_end() {
        return Ok(None);
    }
    let head = segment.read_at(offset, RECORD_SIZE).await?;
    let Some(entry) = EntryHeader::parse(&head, tag) else {
        return Ok(None);
    };
    let len = entry.header.len;
    if offset + EntryHeader::footprint(len) > geometry.entry_end() {
        return Ok(None);
    }
    let payload = if len == 0 {
        Vec::new()
    } else {
        segment
            .read_at(offset + u64_of(RECORD_SIZE), usize_of(u64::from(len)))
            .await?
            .to_vec()
    };
    Ok(Some(EntryRead {
        entry,
        whole: entry.payload_intact(&payload),
    }))
}

/// Whether `header` (with CRC `stored`) is exactly the entry `record` names.
pub(crate) fn entry_matches(record: &Record, header: &EntryHeader, stored: u32) -> bool {
    header.kind == record.kind
        && header.flags == record.flags
        && header.index == record.index
        && header.count == record.count
        && header.batch == record.batch
        && header.position == record.position
        && header.id == record.id
        && header.len == record.len
        && stored == record.entry_crc
}

/// The record an intact entry at `offset` stands for.
fn rebuild(header: &EntryHeader, stored: u32, offset: u64) -> Record {
    Record {
        kind: header.kind,
        flags: header.flags,
        index: header.index,
        count: header.count,
        batch: header.batch,
        position: header.position,
        id: header.id,
        offset: u32::try_from(offset).expect("geometry bounds offsets to u32"),
        len: header.len,
        entry_crc: stored,
    }
}

/// What the metainfo says about the log.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Known {
    /// The open generation of the metainfo: every batch was written by
    /// this generation or an earlier one.
    pub generation: u32,
    /// The last batch the last open found. A batch of an earlier
    /// generation numbered above it is garbage: written beyond what that
    /// open found, so already declared absent.
    pub last: u64,
    /// The high-water mark: a batch the metainfo proves durable.
    pub mark: u64,
    /// The metainfo's `seq`: every entry was written under it or an
    /// earlier one.
    pub seq: u64,
}

impl Known {
    fn is_stale(self, batch: u64) -> bool {
        generation_of(batch) < u64::from(self.generation) && batch > self.last
    }

    /// [`is_stale`](Self::is_stale), or older than the segment that holds
    /// it (`first`): a recycled file's old batch.
    fn is_stale_in(self, first: u64, batch: u64) -> bool {
        batch < first || self.is_stale(batch)
    }

    /// [`check_generation`](Self::check_generation) for every valid record
    /// of a persist log that is not stale. `written`: its slots that are not
    /// all zeros ([`written_slots`]).
    fn check_log(self, log: &[u8], written: &[usize], first: u64) -> Result<(), OpenError> {
        for &at in written {
            if let Slot::Valid(record) = Record::decode(&log[at * RECORD_SIZE..])
                && !self.is_stale_in(first, record.batch)
            {
                self.check_generation(record.batch)?;
            }
        }
        Ok(())
    }

    /// An intact entry header stamped with a later metainfo than this one:
    /// a batch is written under the metainfo that is durable, so the
    /// metainfo lost an update (both copies of it lost).
    fn check_stamp(self, header: &EntryHeader) -> Result<(), OpenError> {
        if stamp_is_ahead(header.stamp, self.seq) {
            return Err(OpenError::MetaUpdateLost {
                seq: self.seq,
                stamp: header.stamp,
                batch: header.batch,
            });
        }
        Ok(())
    }

    /// A batch of a later open generation than the metainfo's: every open
    /// raises the generation durably before it writes, so the metainfo is
    /// older than the log (two faults lost its newer copies).
    fn check_generation(self, batch: u64) -> Result<(), OpenError> {
        if generation_of(batch) > u64::from(self.generation) {
            return Err(OpenError::MetaRolledBack {
                generation: self.generation,
                batch,
            });
        }
        Ok(())
    }
}

/// The slots of a persist log that are not all zeros, in order. A slot of
/// zeros was never written ([`Slot::Zero`]): the scan never needs to decode
/// it, and most of a persist log is zeros. A block of zeros is skipped
/// whole.
fn written_slots(log: &[u8]) -> Vec<usize> {
    let mut written = Vec::new();
    for (block, bytes) in log.chunks(usize_of(BLOCK_U64)).enumerate() {
        if is_zero(bytes) {
            continue;
        }
        let first = block * RECORDS_PER_BLOCK;
        for (at, slot) in bytes.as_chunks::<RECORD_SIZE>().0.iter().enumerate() {
            if !is_zero(slot) {
                written.push(first + at);
            }
        }
    }
    written
}

/// The batch whose first entry is at `offset`, from that entry: when
/// every record of a batch is damaged. `(tag, last)`: the journal's tag and
/// the last batch before it.
async fn batch_from_entry<F: LocalStorageFile>(
    segment: &Segment<F>,
    offset: u64,
    (tag, last): (u32, u64),
    known: Known,
) -> Result<Option<(u64, u16)>, OpenError> {
    // An intact header is enough: it names its batch whole, payload or not.
    match read_entry(segment, offset, tag).await? {
        Some(EntryRead {
            entry: ParsedEntry { header, .. },
            ..
        }) if header.index == 0
            && header.batch > last
            && header.batch >= segment.first_batch
            && !known.is_stale(header.batch) =>
        {
            known.check_generation(header.batch)?;
            known.check_stamp(&header)?;
            Ok(Some((header.batch, header.count)))
        }
        _ => Ok(None),
    }
}

/// Whether a valid record that locates a batch at a boundary is at its own
/// place. The first record of a batch names the first entry of the batch,
/// which is where the entries of the boundary start: a record that names
/// another offset was written to the wrong block (a misdirected write). A
/// later record has no offset to compare with: it is taken as it is.
fn at_its_place(record: &Record, entry_cursor: u64) -> bool {
    record.index != 0 || u64::from(record.offset) == entry_cursor
}

/// The batch at a boundary where no record or first entry locates one: a
/// batch whose first slot is retired. The first retired batch after `last`
/// in this segment, if what lies after the boundary agrees: the first
/// valid record there (`first_valid`, at slot `j`) is of a later batch,
/// beyond the retired batch's persist blocks.
///
/// The retire records come from every segment, so a retired batch can be
/// the first batch of a later segment. `end` is the last batch of this
/// segment as the next segment's header records it (`None` for the last
/// segment): a retired batch above it is not in this segment.
fn batch_from_retire(
    retires: &Retires,
    (last, first_batch, end): (u64, u64, Option<u64>),
    known: Known,
    (at, first_valid): (usize, Option<(usize, Record)>),
) -> Option<(u64, u16)> {
    let retire = retires
        .range((last + 1, 0)..)
        .map(|(_, retire)| retire)
        .find(|retire| retire.index == 0)?;
    let span = usize_of(Record::blocks_for(usize::from(retire.count)))
        * (usize_of(BLOCK_U64) / RECORD_SIZE);
    let agrees =
        first_valid.is_none_or(|(j, record)| record.batch > retire.batch && j >= at + span);
    let in_segment = retire.batch >= first_batch && end.is_none_or(|end| retire.batch <= end);
    (agrees && in_segment && !known.is_stale(retire.batch)).then_some((retire.batch, retire.count))
}

/// The batch at boundary `at` (slot index), with `first_valid` the first
/// valid record at or after it: from a valid record that belongs at this
/// boundary, else from its first entry (every record damaged), else from a
/// retire record that names its first slot. `None` at the end of the log.
/// `(last, entry_cursor)`: the batch before it, and where its entries
/// start. `end`: the last batch of this segment, when a later segment
/// records it.
async fn locate_batch<F: LocalStorageFile>(
    segment: &Segment<F>,
    (at, first_valid): (usize, Option<(usize, Record)>),
    (last, entry_cursor): (u64, u64),
    end: Option<u64>,
    context: Context<'_>,
) -> Result<Option<(u64, u16)>, OpenError> {
    let Context {
        tag,
        known,
        retires,
    } = context;
    // A record can lie before its own place (a misdirected write), so its
    // index can be above its slot.
    if let Some((j, record)) = first_valid
        && j.checked_sub(usize::from(record.index)) == Some(at)
        && at_its_place(&record, entry_cursor)
    {
        return Ok(Some((record.batch, record.count)));
    }
    let shape = match batch_from_entry(segment, entry_cursor, (tag, last), known).await? {
        Some(shape) => Some(shape),
        None => batch_from_retire(
            retires,
            (last, segment.first_batch, end),
            known,
            (at, first_valid),
        ),
    };
    // Something valid lies further on, but nothing locates a batch at this
    // boundary: the batch here is gone, not the end of the log.
    if shape.is_none() && first_valid.is_some() {
        return Err(OpenError::LostBatch { batch: last + 1 });
    }
    Ok(shape)
}

/// What the entry at `offset` says of slot `index` of batch `seq` (of
/// `count` slots), whose record is damaged: its record rebuilt when the
/// entry is intact, read from the header alone when only the payload of a
/// put is damaged, else lost.
async fn from_entry<F: LocalStorageFile>(
    segment: &Segment<F>,
    offset: u64,
    (seq, index, count): (u64, u16, u16),
    (tag, known): (u32, Known),
) -> Result<Found, OpenError> {
    match read_entry(segment, offset, tag).await? {
        Some(EntryRead { entry, whole })
            if entry.header.batch == seq
                && entry.header.index == index
                && entry.header.count == count =>
        {
            known.check_stamp(&entry.header)?;
            let record = rebuild(&entry.header, entry.crc, offset);
            Ok(if whole {
                Found::Rebuilt(record)
            } else if record.kind == Kind::Put {
                Found::Headed(record)
            } else {
                Found::Lost
            })
        }
        _ => Ok(Found::Lost),
    }
}

/// The most persist blocks a sealed segment's scan reads first (1 MiB):
/// most of a sealed segment's persist log is never written. A smaller log
/// reads a 32nd of itself first, so the small test shapes take the same
/// path.
const FIRST_READ_BLOCKS: u64 = 256;

/// Pass 1 over one segment. `prev` is the last batch before it; `end`, its
/// last batch as the next segment's header records it (`None` for the last
/// segment).
///
/// A sealed segment holds no batch after `end`: a roll starts the next
/// segment, and no batch goes to this one again. So its scan reads a prefix
/// of the persist log first, and takes the result only when that prefix
/// holds every batch up to `end` whole. Anything else (a batch that runs
/// past the prefix, a lost batch, a scan that stops short of `end`) reads a
/// prefix four times as long, then the whole log, and scans again. The
/// last segment's log is read whole: nothing says where it ends.
async fn scan_segment<F: LocalStorageFile>(
    segment: &Segment<F>,
    (prev, end): (u64, Option<u64>),
    context: Context<'_>,
) -> Result<(Vec<BatchScan>, u64, u64), OpenError> {
    let whole = u64::from(segment.geometry.persist_blocks);
    let mut blocks = if end.is_some() {
        (whole / 32).clamp(1, FIRST_READ_BLOCKS).min(whole)
    } else {
        whole
    };
    loop {
        let log = segment.read_persist_blocks(blocks).await?;
        let scanned = scan_log(segment, &log, (prev, end), context).await;
        if blocks == whole {
            return scanned;
        }
        if let Ok((batches, _, _)) = &scanned
            && batches.last().map_or(prev, |batch| batch.seq) == end.unwrap_or(prev)
        {
            return scanned;
        }
        blocks = (blocks * 4).min(whole);
    }
}

/// [`scan_segment`] over `log`, the first blocks of its persist log. The
/// scan stops after batch `end`, so the result of a sealed segment does not
/// depend on how much of its log was read past that batch.
async fn scan_log<F: LocalStorageFile>(
    segment: &Segment<F>,
    log: &[u8],
    (prev, end): (u64, Option<u64>),
    context: Context<'_>,
) -> Result<(Vec<BatchScan>, u64, u64), OpenError> {
    let Context {
        tag,
        known,
        retires,
    } = context;
    let geometry = segment.geometry;
    let slots = log.len() / RECORD_SIZE;
    // A stale record reads as damage: it is not evidence of anything. That
    // covers a batch older than the segment, left in a recycled file, and
    // the rule of position: a record at or after the boundary the scan is
    // at whose batch is not above `last` (the last batch found so far).
    let slot = |at: usize, last: u64| match Record::decode(&log[at * RECORD_SIZE..]) {
        Slot::Valid(record)
            if record.batch <= last || known.is_stale_in(segment.first_batch, record.batch) =>
        {
            Slot::Damaged
        }
        slot => slot,
    };
    let written = written_slots(log);
    // A record of a later generation than the metainfo's refuses the open.
    // The last segment checks its whole log; a sealed segment checks only
    // its batches (below), as nothing after `end` is part of it.
    if end.is_none() {
        known.check_log(log, &written, segment.first_batch)?;
    }
    let persist_at = |at: usize| Geometry::PERSIST_START + u64_of(at * RECORD_SIZE);

    let mut batches = Vec::new();
    let mut last = prev;
    let mut at = 0usize; // slot index of the next batch boundary
    let mut entry_cursor = geometry.entry_start();
    // Nothing after a sealed segment's last batch is part of it.
    while at < slots && end != Some(last) {
        // The batch here, from the first valid record at or after `at`.
        let first_valid = written[written.partition_point(|&j| j < at)..]
            .iter()
            .find_map(|&j| match slot(j, last) {
                Slot::Valid(record) => Some((j, record)),
                _ => None,
            });
        let shape = locate_batch(
            segment,
            (at, first_valid),
            (last, entry_cursor),
            end,
            context,
        )
        .await?;
        let Some((seq, count)) = shape else {
            break;
        };
        follows(last, seq)?;
        let batch_slots = usize::from(count);
        if at + batch_slots > slots {
            return Err(OpenError::LostBatch { batch: seq });
        }
        let mut found = Vec::with_capacity(batch_slots);
        let mut walk = Some(entry_cursor);
        let mut entries_end = entry_cursor;
        for index in 0..count {
            let here = match slot(at + usize::from(index), last) {
                Slot::Valid(record)
                    if record.batch == seq && record.index == index && record.count == count =>
                {
                    Found::Record(record)
                }
                _ => match walk {
                    Some(offset) => {
                        from_entry(segment, offset, (seq, index, count), (tag, known)).await?
                    }
                    None => Found::Lost,
                },
            };
            walk = match here.record() {
                Some(record) => {
                    let end = u64::from(record.offset) + EntryHeader::footprint(record.len);
                    entries_end = entries_end.max(end);
                    Some(end)
                }
                // A retired slot says where the walk goes on, and the
                // batch's entries reach at least that far.
                None => retires
                    .get(&(seq, index))
                    .and_then(|retire| (retire.resume != 0).then_some(retire.resume))
                    .inspect(|&resume| entries_end = entries_end.max(resume)),
            };
            found.push(here);
        }
        batches.push(BatchScan {
            seq,
            count,
            persist_at: persist_at(at),
            slots: found,
        });
        last = seq;
        at += usize_of(Record::blocks_for(batch_slots)) * (usize_of(BLOCK_U64) / RECORD_SIZE);
        entry_cursor = align_up(entries_end, BLOCK_U64);
    }
    if end.is_some() {
        let inside = written.partition_point(|&j| j < at);
        known.check_log(log, &written[..inside], segment.first_batch)?;
    }
    let persist_next = Geometry::PERSIST_START + u64_of(at * RECORD_SIZE);
    Ok((batches, persist_next, entry_cursor))
}

/// Batch numbers have no gaps: a generation counts on from the batch
/// before `seq`, and a new one starts at its first number. A gap is a batch
/// the disk lost (a synced write rolled back), and with it, maybe, the
/// retire records that discarded what lies before it: `LostBatch`, even
/// when the walk of the entries went straight past it.
fn follows(last: u64, seq: u64) -> Result<(), OpenError> {
    let expected = if generation_of(seq) == generation_of(last) {
        last + 1
    } else {
        first_batch_of(generation_of(seq))
    };
    if seq == expected {
        Ok(())
    } else {
        Err(OpenError::LostBatch { batch: expected })
    }
}

/// Pass 1 over every segment (sorted by first batch): per segment, its
/// batches and its two cursors; and the last batch in the log.
async fn pass_one<F: LocalStorageFile>(
    segments: &[Segment<F>],
    context: Context<'_>,
) -> Result<(PerSegment, u64), OpenError> {
    let known = context.known;
    let mut per_segment = Vec::with_capacity(segments.len());
    let mut last = segments.first().map_or(0, |s| s.prev_batch);
    for (at, segment) in segments.iter().enumerate() {
        known.check_generation(segment.first_batch)?;
        if segment.prev_batch != last {
            return Err(OpenError::LostBatch { batch: last + 1 });
        }
        // The next segment's header records this segment's last batch.
        let end = segments.get(at + 1).map(|next| next.prev_batch);
        let (batches, persist_next, entry_next) =
            scan_segment(segment, (last, end), context).await?;
        if let Some(batch) = batches.last() {
            last = batch.seq;
        }
        per_segment.push((batches, persist_next, entry_next));
    }
    Ok((per_segment, last))
}

/// Pass 1 with the retire records of the log: run without them first, and
/// again with the ones found while that changes what pass 1 finds. Only a
/// second fault near a retired slot needs the second run: a lost slot that
/// a retire record names, with a lost slot after it in the same batch (the
/// walk broke there, and the batch's entry cursor may be short), or a batch
/// that no record or entry locates (its first slot may be retired).
async fn pass_one_retired<F: LocalStorageFile>(
    segments: &[Segment<F>],
    tag: u32,
    known: Known,
) -> Result<(PerSegment, u64, Retires), OpenError> {
    let mut used = Retires::new();
    loop {
        let context = Context {
            tag,
            known,
            retires: &used,
        };
        let more = match pass_one(segments, context).await {
            Ok((per_segment, last)) => {
                let found = retires_in(&per_segment);
                if !breaks_a_walk(&per_segment, &used, &found) {
                    return Ok((per_segment, last, found));
                }
                found
            }
            Err(OpenError::LostBatch { batch }) => {
                let found = valid_retires(segments, known).await?;
                if found.keys().all(|key| used.contains_key(key)) {
                    return Err(OpenError::LostBatch { batch });
                }
                found
            }
            Err(error) => return Err(error),
        };
        let before = used.len();
        used.extend(more);
        assert!(used.len() > before, "each new run of pass 1 knows more");
    }
}

/// The retire records pass 1 found, valid or rebuilt.
fn retires_in(per_segment: &PerSegment) -> Retires {
    per_segment
        .iter()
        .flat_map(|(batches, _, _)| batches)
        .flat_map(|batch| &batch.slots)
        .filter_map(|found| found.record().and_then(Retire::of))
        .map(|retire| ((retire.batch, retire.index), retire))
        .collect()
}

/// Whether a retire record in `found` and not in `used` names a lost slot
/// that has a lost slot after it in its batch.
fn breaks_a_walk(per_segment: &PerSegment, used: &Retires, found: &Retires) -> bool {
    per_segment
        .iter()
        .flat_map(|(batches, _, _)| batches)
        .any(|batch| {
            let lost: Vec<u16> = (0u16..)
                .zip(&batch.slots)
                .filter(|(_, found)| matches!(found, Found::Lost))
                .map(|(index, _)| index)
                .collect();
            lost.iter().any(|&index| {
                let key = (batch.seq, index);
                found.contains_key(&key)
                    && !used.contains_key(&key)
                    && lost.last().is_some_and(|&after| after > index)
            })
        })
}

/// Every valid retire record of the persist logs, read without pass 1:
/// what a run of pass 1 that met a lost batch could not collect.
async fn valid_retires<F: LocalStorageFile>(
    segments: &[Segment<F>],
    known: Known,
) -> Result<Retires, OpenError> {
    let mut retires = Retires::new();
    for segment in segments {
        let log = segment.read_persist_log().await?;
        for at in written_slots(&log) {
            if let Slot::Valid(record) = Record::decode(&log[at * RECORD_SIZE..])
                && !known.is_stale_in(segment.first_batch, record.batch)
                && let Some(retire) = Retire::of(&record)
            {
                retires.insert((retire.batch, retire.index), retire);
            }
        }
    }
    Ok(retires)
}

/// The persist blocks a scrub of an open journal writes again: pass 1 over
/// every segment, then [`repair_batch`] for each batch up to `last`, the
/// last batch the journal committed (a batch beyond it is a failed commit's,
/// never acknowledged).
pub(crate) async fn plan_repair<F: LocalStorageFile>(
    segments: &[Segment<F>],
    tag: u32,
    known: Known,
    last: u64,
) -> Result<(Vec<BlockWrite>, RecordRepair), OpenError> {
    let (per_segment, _, retires) = pass_one_retired(segments, tag, known).await?;
    let mut writes = Vec::new();
    let mut repair = RecordRepair::default();
    for (s, (batches, _, _)) in per_segment.iter().enumerate() {
        for batch in batches.iter().filter(|batch| batch.seq <= last) {
            let at = (&segments[s], s);
            // An open retired every slot a crash tore: what is left
            // unretired here is corruption, which a write-back can not
            // turn into a witness against a torn slot. But no open
            // generation is known here, so be as careful as an open.
            let torn = (batch, true);
            repair_batch(at, torn, (tag, &retires), &mut writes, &mut repair).await?;
        }
    }
    Ok((writes, repair))
}

/// Scan every segment (sorted by first batch) and judge every record.
pub(crate) async fn scan<F: LocalStorageFile>(
    segments: &[Segment<F>],
    tag: u32,
    known: Known,
) -> Result<Scan, OpenError> {
    let (per_segment, last, retires) = pass_one_retired(segments, tag, known).await?;
    let mut out = Scan {
        last_batch: last,
        ..Scan::default()
    };
    // Each batch's successor in the log, to tell the last batch of an open
    // generation.
    let order: Vec<u64> = per_segment
        .iter()
        .flat_map(|(batches, _, _)| batches.iter().map(|batch| batch.seq))
        .collect();
    let mut next = order.iter().skip(1).copied();
    for (s, (batches, persist_next, entry_next)) in per_segment.into_iter().enumerate() {
        out.cursors.push((persist_next, entry_next));
        let segment = &segments[s];
        for batch in batches {
            let successor = next.next();
            let judge = Judge {
                is_last: batch.seq == last,
                last_of_generation: successor
                    .is_none_or(|after| generation_of(after) != generation_of(batch.seq)),
                tag,
                known,
            };
            judge_batch(segment, &batch, judge, &retires, &mut out).await?;
            let repair = &mut out.repair;
            repair_batch(
                (segment, s),
                (&batch, judge.crash_explains_lost(&batch)),
                (tag, &retires),
                &mut out.writes,
                repair,
            )
            .await?;
        }
    }
    Ok(out)
}

/// What pass 2 knows of a batch beyond its slots.
#[derive(Clone, Copy)]
struct Judge {
    /// The batch is the last in the log: its entries are read now.
    is_last: bool,
    /// The batch is the last that its open generation wrote.
    last_of_generation: bool,
    tag: u32,
    /// What the metainfo says: its high-water mark and its `seq`.
    known: Known,
}

impl Judge {
    /// Whether a crash can explain a slot of `batch` whose record and entry
    /// are both damaged: only in the last batch of its open generation (a
    /// failed commit poisons the journal, so a crash tears no other), above
    /// the mark, and not in an `Ordered` batch that kept an intact record
    /// (its entries were durable before any record was written, so a
    /// damaged one is corruption).
    ///
    /// The open that retires such slots leaves the mark below the batch
    /// until the retire batch is durable, so an open that a crash cut, or a
    /// retire batch that a crash tore, leaves the slots judged the same
    /// way again.
    fn crash_explains_lost(self, batch: &BatchScan) -> bool {
        let ordered_witness = batch.slots.iter().any(
            |found| matches!(found, Found::Record(record) if record.flags & FLAG_ORDERED != 0),
        );
        self.last_of_generation && batch.seq > self.known.mark && !ordered_witness
    }
}

/// The offset of the first located entry after slot `index` of `batch`: where
/// the entry walk goes on after it. 0 when none follows.
fn resume_after(batch: &BatchScan, index: u16) -> u64 {
    batch
        .slots
        .iter()
        .skip(usize::from(index) + 1)
        .find_map(Found::record)
        .map_or(0, |record| u64::from(record.offset))
}

/// Whether the entry of a valid record of the last batch is damaged. An
/// intact header that the record names has its stamp checked.
async fn last_entry_damaged<F: LocalStorageFile>(
    segment: &Segment<F>,
    record: &Record,
    (tag, known): (u32, Known),
) -> Result<bool, OpenError> {
    Ok(
        match read_entry(segment, u64::from(record.offset), tag).await? {
            Some(read) => {
                if entry_matches(record, &read.entry.header, read.entry.crc) {
                    known.check_stamp(&read.entry.header)?;
                }
                !read.proves(record)
            }
            None => true,
        },
    )
}

/// Pass 2 for one batch.
async fn judge_batch<F: LocalStorageFile>(
    segment: &Segment<F>,
    batch: &BatchScan,
    judge: Judge,
    retires: &Retires,
    out: &mut Scan,
) -> Result<(), OpenError> {
    let Judge {
        is_last,
        tag,
        known,
        ..
    } = judge;
    let mark = known.mark;
    let crash_explains_lost = judge.crash_explains_lost(batch);
    for (index, found) in (0u16..).zip(batch.slots.iter()) {
        let record = match *found {
            Found::Rebuilt(record) => {
                out.rebuilt += 1;
                record
            }
            // Named by a retire record: nothing the caller needs.
            Found::Lost | Found::Headed(_) if retires.contains_key(&(batch.seq, index)) => {
                continue;
            }
            Found::Lost | Found::Headed(_) if crash_explains_lost => {
                // Torn by a crash: never acknowledged, discarded. The open
                // retires it before anything vouches for the batch.
                out.torn.push(Retire {
                    batch: batch.seq,
                    index,
                    count: batch.count,
                    resume: resume_after(batch, index),
                });
                continue;
            }
            // A `Headed` slot here is acknowledged, and its header still
            // names it: corruption of a known entry, which a peer can
            // repair. (Its record is not written back: the header proves no
            // payload.)
            Found::Headed(record) => {
                // The header says where its entry ends: the walk goes on
                // there.
                let end = u64::from(record.offset) + EntryHeader::footprint(record.len);
                out.headed.push((
                    record.position,
                    Retire {
                        batch: batch.seq,
                        index,
                        count: batch.count,
                        resume: end,
                    },
                ));
                record
            }
            Found::Record(record) => record,
            Found::Lost => {
                return Err(OpenError::DoubleFault {
                    batch: batch.seq,
                    index,
                });
            }
        };
        // The last batch's entries are read now: it is where a crash and
        // corruption look alike, and it holds the latest stamps.
        let entry_damaged = match *found {
            Found::Headed(_) => true,
            Found::Record(_) if is_last => {
                last_entry_damaged(segment, &record, (tag, known)).await?
            }
            _ => false,
        };
        match record.kind {
            Kind::Retire => {}
            Kind::Clear => out.ops.push(Op::Clear {
                start: record.position,
                end: record.clear_end(),
            }),
            Kind::Put => {
                let damaged = entry_damaged;
                if damaged {
                    // Durable before the batch's records (`Ordered`), or
                    // recorded by the metainfo: the record proves the entry
                    // was synced. A slot read from its header alone got here
                    // only because no crash explains it.
                    if matches!(found, Found::Headed(_))
                        || record.flags & FLAG_ORDERED != 0
                        || batch.seq <= mark
                    {
                        out.corrupt.push((record.position, record.id));
                    } else {
                        out.ambiguous.push((record.position, record.id));
                    }
                }
                out.ops.push(Op::Put {
                    position: record.position,
                    at: Located {
                        segment: segment.first_batch,
                        record_at: batch.persist_at + Record::slot_offset(index),
                        offset: record.offset,
                        len: record.len,
                        entry_crc: record.entry_crc,
                        id: record.id,
                        batch: record.batch,
                        index,
                        count: record.count,
                        flags: record.flags,
                        damaged,
                    },
                });
            }
        }
    }
    Ok(())
}

/// Write back the rebuilt records of `batch`, so the damage of each record
/// is gone before a second fault on its slot makes an open refuse.
///
/// A persist block that holds a rebuilt record is written again whole, as
/// one durable write, but only when each slot in it is proven: its entry is
/// intact and is the entry its record names, or a retire record names the
/// slot (its record is written back as it is, or left zero when it is
/// lost). Then a crash that tears the write leaves each record of the block
/// rebuildable from its entry, or retired, as before the write. A block
/// with one damaged entry is not written, because its valid record is the
/// only copy of that identity; it is counted as left.
///
/// A batch with a lost slot that no retire record names, in a batch where
/// a crash explains a loss, is not written at all: its torn slots are
/// retired first. A rebuilt `Ordered` record
/// written back beside them would read as a witness that makes them
/// corruption, if a crash came before the retire batch is durable.
async fn repair_batch<F: LocalStorageFile>(
    (segment, segment_index): (&Segment<F>, usize),
    (batch, crash_explains_lost): (&BatchScan, bool),
    (tag, retires): (u32, &Retires),
    writes: &mut Vec<BlockWrite>,
    repair: &mut RecordRepair,
) -> std::io::Result<()> {
    let named = |index: usize| {
        u16::try_from(index).is_ok_and(|index| retires.contains_key(&(batch.seq, index)))
    };
    // A slot read from its header alone is torn only where a crash
    // explains a loss; elsewhere it is corruption, and blocks nothing.
    let lost = |found: &Found| match found {
        Found::Lost => true,
        Found::Headed(_) => crash_explains_lost,
        Found::Record(_) | Found::Rebuilt(_) => false,
    };
    let unretired_loss =
        (0..batch.slots.len()).any(|index| lost(&batch.slots[index]) && !named(index));
    for (block, slots) in (0u64..).zip(batch.slots.chunks(RECORDS_PER_BLOCK)) {
        if !slots.iter().any(|found| matches!(found, Found::Rebuilt(_))) {
            continue;
        }
        let first = usize_of(block) * RECORDS_PER_BLOCK;
        let proven =
            !unretired_loss && entries_prove(segment, slots, tag, |at| named(first + at)).await?;
        if proven {
            writes.push((
                segment_index,
                batch.persist_at + block * BLOCK_U64,
                encode_records(slots.iter().map(Found::proven)),
            ));
            repair.blocks_written += 1;
        } else {
            repair.blocks_left += 1;
        }
    }
    Ok(())
}

/// Whether each slot of `slots` is proven: a rebuilt record was made from an
/// intact entry, a retired slot (`retired`, by its place in `slots`) holds
/// nothing the caller needs, and a valid record needs its intact entry.
async fn entries_prove<F: LocalStorageFile>(
    segment: &Segment<F>,
    slots: &[Found],
    tag: u32,
    retired: impl Fn(usize) -> bool,
) -> std::io::Result<bool> {
    for (at, found) in slots.iter().enumerate() {
        match found {
            Found::Rebuilt(_) => {}
            _ if retired(at) => {}
            Found::Record(record) => {
                match read_entry(segment, u64::from(record.offset), tag).await? {
                    Some(read) if read.proves(record) => {}
                    _ => return Ok(false),
                }
            }
            Found::Headed(_) | Found::Lost => return Ok(false),
        }
    }
    Ok(true)
}

/// A batch's persist extent: each record encoded in its slot, a `None`
/// slot left zero (a never-written slot).
fn encode_records<'a>(slots: impl ExactSizeIterator<Item = Option<&'a Record>>) -> AlignedBuf {
    let blocks = Record::blocks_for(slots.len());
    let mut bytes = crate::io::block_buf(usize_of(blocks * BLOCK_U64));
    for (at, record) in slots.enumerate() {
        if let Some(record) = record {
            record.encode(&mut bytes[at * RECORD_SIZE..]);
        }
    }
    bytes
}
