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
//! **Pass 2, verdicts** (the table in the crate docs). Only the **last
//! batch** and damaged records cost entry reads: the rest of the log is
//! checked when it is read.
//!
//! **What the metainfo proves.** Its high-water mark ([`Known::mark`]) is a
//! batch known durable: a slot of a batch at or below it whose record and
//! entry are both damaged is corruption, never a crash. Its open generation
//! ([`Known::generation`]) is above every batch number written so far: a
//! batch of a later generation proves the metainfo older than the log
//! ([`OpenError::MetaRolledBack`]).

use moonpool_core::{AlignedBuf, LocalStorageFile};

use crate::OpenError;
use crate::format::{
    BLOCK_U64, EntryHeader, FLAG_ORDERED, Geometry, Id, Kind, RECORD_SIZE, Record, Slot, align_up,
    entry_crc, generation_of, u64_of, usize_of,
};
use crate::segment::Segment;

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
    pub voided: u32,
    /// The last batch's persist extent rewritten: `(segment index, offset,
    /// bytes)`. Set when it voids or rebuilds a record, so the next open
    /// finds a durable verdict instead of damage before a later batch.
    /// Written in order, each durable before the next: when the batch has
    /// both voids and rebuilt records, the voids land first (see
    /// [`judge_batch`]).
    pub rewrite: Option<(usize, u64, Vec<AlignedBuf>)>,
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

/// Pass 1 over one segment. `prev` is the last batch before it.
async fn scan_segment<F: LocalStorageFile>(
    segment: &Segment<F>,
    prev: u64,
    tag: u32,
    known: Known,
) -> Result<(Vec<BatchScan>, u64, u64), OpenError> {
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
        // The batch here: from a valid record that belongs at this
        // boundary, else from its first entry (every record damaged). A
        // record can lie before its own place (a misdirected write), so its
        // index can be above its slot.
        let from_record = match first_valid {
            Some((j, record)) if j.checked_sub(usize::from(record.index)) == Some(at) => {
                if record.batch <= last || record.batch < segment.first_batch {
                    return Err(OpenError::LostBatch { batch: last + 1 });
                }
                Some((record.batch, record.count))
            }
            _ => None,
        };
        let shape = match from_record {
            Some(shape) => Some(shape),
            None => batch_from_entry(segment, entry_cursor, (tag, last), known).await?,
        };
        // Something valid lies further on, but nothing locates a batch at
        // this boundary: the batch here is gone, not the end of the log.
        if shape.is_none() && first_valid.is_some() {
            return Err(OpenError::LostBatch { batch: last + 1 });
        }
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
                Found::Record(record) | Found::Rebuilt(record) if record.kind != Kind::Void => {
                    let end = u64::from(record.offset) + EntryHeader::footprint(record.len);
                    entries_end = entries_end.max(end);
                    Some(end)
                }
                _ => None,
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

/// Scan every segment (sorted by first batch) and judge every record.
pub(crate) async fn scan<F: LocalStorageFile>(
    segments: &[Segment<F>],
    tag: u32,
    known: Known,
) -> Result<Scan, OpenError> {
    let mut per_segment = Vec::with_capacity(segments.len());
    let mut last = segments.first().map_or(0, |s| s.prev_batch);
    for segment in segments {
        known.check_generation(segment.first_batch)?;
        if segment.prev_batch != last {
            return Err(OpenError::LostBatch { batch: last + 1 });
        }
        let (batches, persist_next, entry_next) = scan_segment(segment, last, tag, known).await?;
        if let Some(batch) = batches.last() {
            last = batch.seq;
        }
        per_segment.push((batches, persist_next, entry_next));
    }

    let mut out = Scan {
        last_batch: last,
        ..Scan::default()
    };
    let last_location = per_segment
        .iter()
        .enumerate()
        .rev()
        .find_map(|(s, (batches, _, _))| (!batches.is_empty()).then(|| (s, batches.len() - 1)));
    for (s, (batches, persist_next, entry_next)) in per_segment.into_iter().enumerate() {
        out.cursors.push((persist_next, entry_next));
        let segment = &segments[s];
        for (b, batch) in batches.into_iter().enumerate() {
            let is_last = last_location == Some((s, b));
            let judge = Judge {
                is_last,
                tag,
                mark: known.mark,
            };
            judge_batch(segment, s, batch, judge, &mut out).await?;
        }
    }
    Ok(out)
}

/// What pass 2 knows of a batch beyond its slots.
#[derive(Clone, Copy)]
struct Judge {
    /// The batch is the last in the log.
    is_last: bool,
    tag: u32,
    /// The metainfo's high-water mark.
    mark: u64,
}

impl Judge {
    /// Whether a crash can explain a slot of `batch` whose record and entry
    /// are both damaged: only in the last batch, above the mark, and not in
    /// an `Ordered` batch that kept an intact record (its entries were
    /// durable before any record was written, so a damaged one is
    /// corruption).
    fn crash_explains_lost(self, batch: &BatchScan) -> bool {
        let ordered_witness = batch.slots.iter().any(
            |found| matches!(found, Found::Record(record) if record.flags & FLAG_ORDERED != 0),
        );
        self.is_last && batch.seq > self.mark && !ordered_witness
    }
}

/// Pass 2 for one batch.
async fn judge_batch<F: LocalStorageFile>(
    segment: &Segment<F>,
    segment_index: usize,
    batch: BatchScan,
    judge: Judge,
    out: &mut Scan,
) -> Result<(), OpenError> {
    let Judge { is_last, tag, mark } = judge;
    let crash_explains_lost = judge.crash_explains_lost(&batch);
    let mut rewrite = false;
    let mut voided = false;
    let mut rebuilt = false;
    let mut kept: Vec<Record> = Vec::with_capacity(batch.slots.len());
    // What the slots held on disk before this open: the voids-first write
    // keeps a valid record and leaves a rebuilt slot zero.
    let mut on_disk: Vec<Option<Record>> = Vec::with_capacity(batch.slots.len());
    for (index, found) in (0u16..).zip(batch.slots.iter()) {
        let record = match *found {
            Found::Record(record) => {
                on_disk.push(Some(record));
                record
            }
            Found::Rebuilt(record) => {
                out.rebuilt += 1;
                rewrite |= is_last;
                rebuilt = true;
                on_disk.push(None);
                record
            }
            Found::Lost if crash_explains_lost => {
                // Torn by a crash: never acknowledged, discarded.
                out.voided += 1;
                rewrite = true;
                voided = true;
                let void = Record {
                    kind: Kind::Void,
                    flags: 0,
                    index,
                    count: batch.count,
                    batch: batch.seq,
                    position: 0,
                    id: [0; crate::ID_SIZE],
                    offset: 0,
                    len: 0,
                    entry_crc: 0,
                };
                on_disk.push(Some(void));
                kept.push(void);
                continue;
            }
            Found::Lost => {
                return Err(OpenError::DoubleFault {
                    batch: batch.seq,
                    index,
                });
            }
        };
        kept.push(record);
        match record.kind {
            Kind::Void => {}
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
    if rewrite {
        // Voids land before rebuilt records. A rebuilt `Ordered` record is
        // an intact record, which makes a lost slot corruption: if a crash
        // tore one rewrite, a rebuilt record that landed beside a void that
        // did not would read as a double fault. Written first and alone,
        // the voids leave every slot either valid or rebuildable.
        let mut writes = Vec::with_capacity(2);
        if voided && rebuilt {
            writes.push(encode_records(on_disk.iter().map(|slot| slot.as_ref())));
        }
        writes.push(encode_records(kept.iter().map(Some)));
        out.rewrite = Some((segment_index, batch.persist_at, writes));
    }
    Ok(())
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
