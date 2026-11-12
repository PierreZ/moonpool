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
    BLOCK_U64, EntryHeader, FLAG_ORDERED, Geometry, Id, Kind, RECORD_SIZE, RECORDS_PER_BLOCK,
    Record, Retire, Slot, align_up, entry_crc, generation_of, u64_of, usize_of,
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
    /// Both damaged, or the entry cannot be located.
    Lost,
}

impl Found {
    /// The record this slot holds, valid or rebuilt.
    fn record(&self) -> Option<&Record> {
        match self {
            Found::Record(record) | Found::Rebuilt(record) => Some(record),
            Found::Lost => None,
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
    /// The persist blocks the open writes again, in order, each durable
    /// before the next (see [`repair_batch`]).
    pub writes: Vec<BlockWrite>,
    /// The blocks of earlier batches written again, and those left.
    pub repair: RecordRepair,
}

/// An entry checked against what is expected of it.
async fn read_entry<F: LocalStorageFile>(
    segment: &Segment<F>,
    offset: u64,
    tag: u32,
) -> std::io::Result<Option<(EntryHeader, u32, Vec<u8>)>> {
    let geometry = segment.geometry;
    if offset < geometry.entry_start() || offset + u64_of(RECORD_SIZE) > geometry.entry_end() {
        return Ok(None);
    }
    let head = segment.read_at(offset, RECORD_SIZE).await?;
    let Some((header, stored)) = EntryHeader::parse(&head) else {
        return Ok(None);
    };
    if header.tag != tag || offset + EntryHeader::footprint(header.len) > geometry.entry_end() {
        return Ok(None);
    }
    let whole = segment
        .read_at(offset, RECORD_SIZE + usize_of(u64::from(header.len)))
        .await?;
    let payload = whole[RECORD_SIZE..].to_vec();
    if entry_crc(&whole[..RECORD_SIZE], &payload) != stored {
        return Ok(None);
    }
    Ok(Some((header, stored, payload)))
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
    /// of a persist log that is not stale.
    fn check_log(self, log: &[u8], first: u64) -> Result<(), OpenError> {
        for bytes in log.as_chunks::<RECORD_SIZE>().0 {
            if let Slot::Valid(record) = Record::decode(bytes)
                && !self.is_stale_in(first, record.batch)
            {
                self.check_generation(record.batch)?;
            }
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

/// The batch whose first entry is at `offset`, from that entry: when
/// every record of a batch is damaged. `(tag, last)`: the journal's tag and
/// the last batch before it.
async fn batch_from_entry<F: LocalStorageFile>(
    segment: &Segment<F>,
    offset: u64,
    (tag, last): (u32, u64),
    known: Known,
) -> Result<Option<(u64, u16)>, OpenError> {
    match read_entry(segment, offset, tag).await? {
        Some((header, _, _))
            if header.index == 0
                && header.batch > last
                && header.batch >= segment.first_batch
                && !known.is_stale(header.batch) =>
        {
            known.check_generation(header.batch)?;
            Ok(Some((header.batch, header.count)))
        }
        _ => Ok(None),
    }
}

/// The batch at a boundary where no record or first entry locates one: a
/// batch whose first slot is retired. The first retired batch after `last`
/// in this segment, if what lies after the boundary agrees: the first
/// valid record there (`first_valid`, at slot `j`) is of a later batch,
/// beyond the retired batch's persist blocks.
fn batch_from_retire(
    retires: &Retires,
    (last, first_batch): (u64, u64),
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
    (agrees && retire.batch >= first_batch && !known.is_stale(retire.batch))
        .then_some((retire.batch, retire.count))
}

/// The batch at boundary `at` (slot index), with `first_valid` the first
/// valid record at or after it: from a valid record that belongs at this
/// boundary, else from its first entry (every record damaged), else from a
/// retire record that names its first slot. `None` at the end of the log.
/// `(last, entry_cursor)`: the batch before it, and where its entries
/// start.
async fn locate_batch<F: LocalStorageFile>(
    segment: &Segment<F>,
    (at, first_valid): (usize, Option<(usize, Record)>),
    (last, entry_cursor): (u64, u64),
    context: Context<'_>,
) -> Result<Option<(u64, u16)>, OpenError> {
    let Context {
        tag,
        known,
        retires,
    } = context;
    if let Some((j, record)) = first_valid
        && j - usize::from(record.index) == at
    {
        if record.batch <= last || record.batch < segment.first_batch {
            return Err(OpenError::LostBatch { batch: last + 1 });
        }
        return Ok(Some((record.batch, record.count)));
    }
    let shape = match batch_from_entry(segment, entry_cursor, (tag, last), known).await? {
        Some(shape) => Some(shape),
        None => batch_from_retire(
            retires,
            (last, segment.first_batch),
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

/// Pass 1 over one segment. `prev` is the last batch before it.
async fn scan_segment<F: LocalStorageFile>(
    segment: &Segment<F>,
    prev: u64,
    context: Context<'_>,
) -> Result<(Vec<BatchScan>, u64, u64), OpenError> {
    let Context {
        tag,
        known,
        retires,
    } = context;
    let geometry = segment.geometry;
    let log = segment.read_persist_log().await?;
    let slots = log.len() / RECORD_SIZE;
    // A stale record reads as damage: it is not evidence of anything. That
    // covers a batch older than the segment, left in a recycled file.
    let slot = |at: usize| match Record::decode(&log[at * RECORD_SIZE..]) {
        Slot::Valid(record) if known.is_stale_in(segment.first_batch, record.batch) => {
            Slot::Damaged
        }
        slot => slot,
    };
    known.check_log(&log, segment.first_batch)?;
    let persist_at = |at: usize| Geometry::PERSIST_START + u64_of(at * RECORD_SIZE);

    let mut batches = Vec::new();
    let mut last = prev;
    let mut at = 0usize; // slot index of the next batch boundary
    let mut entry_cursor = geometry.entry_start();
    while at < slots {
        // The batch here, from the first valid record at or after `at`.
        let first_valid = (at..slots).find_map(|j| match slot(j) {
            Slot::Valid(record) => Some((j, record)),
            _ => None,
        });
        let shape = locate_batch(segment, (at, first_valid), (last, entry_cursor), context).await?;
        let Some((seq, count)) = shape else {
            break;
        };
        let batch_slots = usize::from(count);
        if at + batch_slots > slots {
            return Err(OpenError::LostBatch { batch: seq });
        }
        let mut found = Vec::with_capacity(batch_slots);
        let mut walk = Some(entry_cursor);
        let mut entries_end = entry_cursor;
        for index in 0..count {
            let here = match slot(at + usize::from(index)) {
                Slot::Valid(record)
                    if record.batch == seq && record.index == index && record.count == count =>
                {
                    Found::Record(record)
                }
                _ => match walk {
                    Some(offset) => match read_entry(segment, offset, tag).await? {
                        Some((header, stored, _))
                            if header.batch == seq
                                && header.index == index
                                && header.count == count =>
                        {
                            Found::Rebuilt(rebuild(&header, stored, offset))
                        }
                        _ => Found::Lost,
                    },
                    None => Found::Lost,
                },
            };
            walk = match here {
                Found::Record(record) | Found::Rebuilt(record) => {
                    let end = u64::from(record.offset) + EntryHeader::footprint(record.len);
                    entries_end = entries_end.max(end);
                    Some(end)
                }
                // A retired slot says where the walk goes on.
                Found::Lost => retires
                    .get(&(seq, index))
                    .and_then(|retire| (retire.resume != 0).then_some(retire.resume)),
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
    let persist_next = Geometry::PERSIST_START + u64_of(at * RECORD_SIZE);
    Ok((batches, persist_next, entry_cursor))
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
    for segment in segments {
        known.check_generation(segment.first_batch)?;
        if segment.prev_batch != last {
            return Err(OpenError::LostBatch { batch: last + 1 });
        }
        let (batches, persist_next, entry_next) = scan_segment(segment, last, context).await?;
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
        for bytes in log.as_chunks::<RECORD_SIZE>().0 {
            if let Slot::Valid(record) = Record::decode(bytes)
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
            repair_batch(at, batch, (tag, &retires), &mut writes, &mut repair).await?;
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
                mark: known.mark,
            };
            judge_batch(segment, &batch, judge, &retires, &mut out).await?;
            let repair = &mut out.repair;
            repair_batch(
                (segment, s),
                &batch,
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
    /// The metainfo's high-water mark.
    mark: u64,
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
        self.last_of_generation && batch.seq > self.mark && !ordered_witness
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

/// Pass 2 for one batch.
async fn judge_batch<F: LocalStorageFile>(
    segment: &Segment<F>,
    batch: &BatchScan,
    judge: Judge,
    retires: &Retires,
    out: &mut Scan,
) -> Result<(), OpenError> {
    let Judge {
        is_last, tag, mark, ..
    } = judge;
    let crash_explains_lost = judge.crash_explains_lost(batch);
    for (index, found) in (0u16..).zip(batch.slots.iter()) {
        let record = match *found {
            Found::Record(record) => record,
            Found::Rebuilt(record) => {
                out.rebuilt += 1;
                record
            }
            // Named by a retire record: nothing the caller needs.
            Found::Lost if retires.contains_key(&(batch.seq, index)) => continue,
            Found::Lost if crash_explains_lost => {
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
            Found::Lost => {
                return Err(OpenError::DoubleFault {
                    batch: batch.seq,
                    index,
                });
            }
        };
        match record.kind {
            Kind::Retire => {}
            Kind::Clear => out.ops.push(Op::Clear {
                start: record.position,
                end: record.clear_end(),
            }),
            Kind::Put => {
                // The last batch's entries are read now: it is where a crash
                // and corruption look alike.
                let damaged = if is_last && matches!(found, Found::Record(_)) {
                    match read_entry(segment, u64::from(record.offset), tag).await? {
                        Some((header, stored, _)) => !entry_matches(&record, &header, stored),
                        None => true,
                    }
                } else {
                    false
                };
                if damaged {
                    // Durable before the batch's records (`Ordered`), or
                    // recorded by the metainfo: the record proves the entry
                    // was synced.
                    if record.flags & FLAG_ORDERED != 0 || batch.seq <= mark {
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
/// A batch with a lost slot that no retire record names is not written at
/// all: its torn slots are retired first. A rebuilt `Ordered` record
/// written back beside them would read as a witness that makes them
/// corruption, if a crash came before the retire batch is durable.
async fn repair_batch<F: LocalStorageFile>(
    (segment, segment_index): (&Segment<F>, usize),
    batch: &BatchScan,
    (tag, retires): (u32, &Retires),
    writes: &mut Vec<BlockWrite>,
    repair: &mut RecordRepair,
) -> std::io::Result<()> {
    let named = |index: usize| {
        u16::try_from(index).is_ok_and(|index| retires.contains_key(&(batch.seq, index)))
    };
    let unretired_loss = (0..batch.slots.len())
        .any(|index| matches!(batch.slots[index], Found::Lost) && !named(index));
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
                encode_records(slots.iter().map(Found::record)),
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
                    Some((header, stored, _)) if entry_matches(record, &header, stored) => {}
                    _ => return Ok(false),
                }
            }
            Found::Lost => return Ok(false),
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
