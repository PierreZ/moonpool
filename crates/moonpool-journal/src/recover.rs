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
//! which nothing valid follows.
//!
//! **Pass 2, verdicts** (the table in the crate docs). Only the **last
//! batch** and damaged records cost entry reads: the rest of the log is
//! checked when it is read.

use moonpool_core::StorageFile;

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
    pub rewrite: Option<(usize, u64, Vec<u8>)>,
}

/// An entry checked against what is expected of it.
async fn read_entry<F: StorageFile>(
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

/// Which batch numbers are garbage: written by an earlier open generation
/// beyond what the next open found, so already declared absent.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stale {
    pub generation: u32,
    pub last: u64,
}

impl Stale {
    fn is_stale(self, batch: u64) -> bool {
        generation_of(batch) < u64::from(self.generation) && batch > self.last
    }
}

/// Pass 1 over one segment. `prev` is the last batch before it.
async fn scan_segment<F: StorageFile>(
    segment: &Segment<F>,
    prev: u64,
    tag: u32,
    stale: Stale,
) -> Result<(Vec<BatchScan>, u64, u64), OpenError> {
    let geometry = segment.geometry;
    let log = segment.read_persist_log().await?;
    let slots = log.len() / RECORD_SIZE;
    // A stale record reads as damage: it is not evidence of anything.
    let slot = |at: usize| match Record::decode(&log[at * RECORD_SIZE..]) {
        Slot::Valid(record) if stale.is_stale(record.batch) => Slot::Damaged,
        slot => slot,
    };
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
        // boundary, else from its first entry (every record damaged).
        let from_record = match first_valid {
            Some((j, record)) if j - usize::from(record.index) == at => {
                if record.batch <= last || record.batch < segment.first_batch {
                    return Err(OpenError::LostBatch { batch: last + 1 });
                }
                Some((record.batch, record.count))
            }
            _ => None,
        };
        let shape = match from_record {
            Some(shape) => Some(shape),
            None => match read_entry(segment, entry_cursor, tag).await? {
                Some((header, _, _))
                    if header.index == 0
                        && header.batch > last
                        && header.batch >= segment.first_batch
                        && !stale.is_stale(header.batch) =>
                {
                    Some((header.batch, header.count))
                }
                _ => None,
            },
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
pub(crate) async fn scan<F: StorageFile>(
    segments: &[Segment<F>],
    tag: u32,
    stale: Stale,
) -> Result<Scan, OpenError> {
    let mut per_segment = Vec::with_capacity(segments.len());
    let mut last = segments.first().map_or(0, |s| s.prev_batch);
    for segment in segments {
        if segment.prev_batch != last {
            return Err(OpenError::LostBatch { batch: last + 1 });
        }
        let (batches, persist_next, entry_next) = scan_segment(segment, last, tag, stale).await?;
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
            judge_batch(segment, s, batch, is_last, tag, &mut out).await?;
        }
    }
    Ok(out)
}

/// Pass 2 for one batch.
async fn judge_batch<F: StorageFile>(
    segment: &Segment<F>,
    segment_index: usize,
    batch: BatchScan,
    is_last: bool,
    tag: u32,
    out: &mut Scan,
) -> Result<(), OpenError> {
    let mut rewrite = false;
    let mut kept: Vec<Record> = Vec::with_capacity(batch.slots.len());
    for (index, found) in (0u16..).zip(batch.slots.iter()) {
        let record = match *found {
            Found::Record(record) => record,
            Found::Rebuilt(record) => {
                out.rebuilt += 1;
                rewrite |= is_last;
                record
            }
            Found::Lost if is_last => {
                // Torn by a crash: never acknowledged, discarded.
                out.voided += 1;
                rewrite = true;
                kept.push(Record {
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
                    if record.flags & FLAG_ORDERED != 0 {
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
        let blocks = Record::blocks_for(kept.len());
        let mut bytes = vec![0u8; usize_of(blocks * BLOCK_U64)];
        for (at, record) in kept.iter().enumerate() {
            record.encode(&mut bytes[at * RECORD_SIZE..]);
        }
        out.rewrite = Some((segment_index, batch.persist_at, bytes));
    }
    Ok(())
}
