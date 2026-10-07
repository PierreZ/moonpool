//! On-disk formats: the geometry, the segment header, the persist record,
//! the entry header and the metainfo copy. Nothing here does I/O: these are
//! the encoders, the decoders and the arithmetic the rest of the crate
//! shares. Every integer is little-endian; every structure carries a CRC32C
//! over everything before it.

use crate::JournalId;

/// The unit of every transfer and the alignment of every region.
pub const BLOCK: usize = 4096;
pub(crate) const BLOCK_U64: u64 = BLOCK as u64;

/// Size of the caller's identity: stored in the persist record, far from
/// the entry, and again in the entry's header.
pub const ID_SIZE: usize = 24;

/// The caller's identity of an entry, opaque to the journal. A Paxos
/// acceptor stores the ballot here, a Raft log the term: whatever recovery
/// must name when the entry's bytes are lost.
pub type Id = [u8; ID_SIZE];

/// Size of a persist record and of an entry header.
pub const RECORD_SIZE: usize = 64;
const RECORD_U64: u64 = RECORD_SIZE as u64;

/// Persist records per block.
pub(crate) const RECORDS_PER_BLOCK: usize = BLOCK / RECORD_SIZE;

/// Most records one batch may hold: the index within a batch is a `u16`.
pub const MAX_BATCH_RECORDS: usize = u16::MAX as usize;

/// Largest caller metainfo, in bytes: one block less the copy's own fields.
pub const META_MAX: usize = META_CRC_AT - META_BYTES_AT;

const META_BYTES_AT: usize = 80;
const META_CRC_AT: usize = BLOCK - 4;

/// Where metainfo copy B lives in the meta file: far from copy A, so one
/// misdirected or local write cannot reach both.
pub(crate) const META_COPY_B_AT: u64 = 16 * BLOCK_U64;
/// The meta file's size: copy A, a gap, copy B.
pub(crate) const META_FILE_BLOCKS: u64 = 17;

const SEGMENT_MAGIC: u32 = u32::from_le_bytes(*b"MPJS");
const ENTRY_MAGIC: u32 = u32::from_le_bytes(*b"MPJE");
const META_MAGIC: u32 = u32::from_le_bytes(*b"MPJM");
const VERSION: u16 = 2;

const _: () = assert!(BLOCK.is_multiple_of(RECORD_SIZE));
const _: () = assert!(RECORDS_PER_BLOCK == 64);
const _: () = assert!(META_MAX >= 4000);
const _: () = assert!(GENERATION_SHIFT >= 32 && GENERATION_SHIFT < 64);
const _: () = assert!(META_COPY_B_AT >= 16 * BLOCK_U64);

fn crc(bytes: &[u8]) -> u32 {
    crc32c::crc32c(bytes)
}

fn put_u16(buf: &mut [u8], at: usize, value: u16) {
    buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn put_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn put_u64(buf: &mut [u8], at: usize, value: u64) {
    buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
fn put_u128(buf: &mut [u8], at: usize, value: u128) {
    buf[at..at + 16].copy_from_slice(&value.to_le_bytes());
}
fn get_u16(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(buf[at..at + 2].try_into().expect("two bytes"))
}
fn get_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().expect("four bytes"))
}
fn get_u64(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().expect("eight bytes"))
}
fn get_u128(buf: &[u8], at: usize) -> u128 {
    u128::from_le_bytes(buf[at..at + 16].try_into().expect("sixteen bytes"))
}

pub(crate) fn align_up(value: u64, to: u64) -> u64 {
    value.div_ceil(to) * to
}

pub(crate) fn u64_of(len: usize) -> u64 {
    u64::try_from(len).expect("usize fits u64")
}

pub(crate) fn usize_of(len: u64) -> usize {
    usize::try_from(len).expect("a journal length fits usize")
}

/// A 32-bit tag of the journal's identity, stamped in every entry header so
/// an entry written by another journal never passes for one of ours.
pub(crate) fn journal_tag(journal: JournalId) -> u32 {
    crc(&journal.0.to_le_bytes())
}

/// The shape of every segment file: the persist log, a guard gap that keeps
/// identifiers physically away from entries, and the entry log.
///
/// ```text
/// block 0          header copy A
/// block 1          header copy B
/// block 2 ..       persist log: 64-byte persist records, 64 per block
/// ..               guard gap: zeros
/// ..  end          entry log: 64-byte entry headers and payloads
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Blocks in the persist log: 64 records each.
    pub persist_blocks: u32,
    /// Blocks of zeros between the persist log and the entry log.
    pub gap_blocks: u32,
    /// Blocks in the entry log.
    pub entry_blocks: u32,
}

impl Default for Geometry {
    /// 32,768 records (2 MiB), a 2 MiB gap, 60 MiB of entries: 64 MiB and
    /// two header blocks per segment.
    fn default() -> Self {
        Self {
            persist_blocks: 512,
            gap_blocks: 512,
            entry_blocks: 15_360,
        }
    }
}

impl Geometry {
    /// A small shape for tests and simulation: 512 records, no gap, 64 KiB
    /// of entries, so rollover comes quickly.
    #[must_use]
    pub fn small() -> Self {
        Self {
            persist_blocks: 8,
            gap_blocks: 0,
            entry_blocks: 16,
        }
    }

    /// Whether this shape describes a usable segment: every region at least
    /// one block, and every entry offset fits the `u32` a record stores.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.persist_blocks >= 1
            && self.entry_blocks >= 1
            && u32::try_from(self.total_blocks() * BLOCK_U64).is_ok()
    }

    /// The persist log follows the two header blocks.
    pub(crate) const PERSIST_START: u64 = 2 * BLOCK_U64;

    pub(crate) fn persist_end(&self) -> u64 {
        Self::PERSIST_START + u64::from(self.persist_blocks) * BLOCK_U64
    }
    pub(crate) fn entry_start(&self) -> u64 {
        self.persist_end() + u64::from(self.gap_blocks) * BLOCK_U64
    }
    pub(crate) fn entry_end(&self) -> u64 {
        self.entry_start() + u64::from(self.entry_blocks) * BLOCK_U64
    }
    pub(crate) fn total_blocks(&self) -> u64 {
        2 + u64::from(self.persist_blocks)
            + u64::from(self.gap_blocks)
            + u64::from(self.entry_blocks)
    }
}

/// A segment's header: written once, in both copies, when it is created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SegmentHeader {
    pub journal: JournalId,
    /// The first batch it was created for (and is named after).
    pub first_batch: u64,
    /// The last batch of the log when it was created: the link to the
    /// segment before it, so a missing segment is never read as an end.
    pub prev_batch: u64,
    pub geometry: Geometry,
}

impl SegmentHeader {
    pub fn encode(&self, block: &mut [u8]) {
        block[..BLOCK].fill(0);
        put_u32(block, 0, SEGMENT_MAGIC);
        put_u16(block, 4, VERSION);
        put_u128(block, 8, self.journal.0);
        put_u64(block, 24, self.first_batch);
        put_u32(block, 32, self.geometry.persist_blocks);
        put_u32(block, 36, self.geometry.gap_blocks);
        put_u32(block, 40, self.geometry.entry_blocks);
        put_u64(block, 44, self.prev_batch);
        put_u32(block, 52, crc(&block[..52]));
        check_round_trip(Self::decode(block) == Some(*self));
    }

    pub fn decode(block: &[u8]) -> Option<Self> {
        if get_u32(block, 0) != SEGMENT_MAGIC
            || get_u16(block, 4) != VERSION
            || get_u32(block, 52) != crc(&block[..52])
        {
            return None;
        }
        Some(Self {
            journal: JournalId(get_u128(block, 8)),
            first_batch: get_u64(block, 24),
            prev_batch: get_u64(block, 44),
            geometry: Geometry {
                persist_blocks: get_u32(block, 32),
                gap_blocks: get_u32(block, 36),
                entry_blocks: get_u32(block, 40),
            },
        })
    }
}

/// An encoder's postcondition: what it wrote decodes back to its input.
/// Checked always (no `debug_assert!`), cheap next to the I/O it precedes.
fn check_round_trip(holds: bool) {
    assert!(holds, "an encoded structure decodes back to itself");
}

/// What a persist record (and the entry it describes) is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// An entry at a position.
    Put = 1,
    /// A tombstone over `position..end`; `end` rides in the id's first word.
    Clear = 2,
    /// A record recovery discarded: a torn write in the last batch.
    Void = 3,
}

impl Kind {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Kind::Put),
            2 => Some(Kind::Clear),
            3 => Some(Kind::Void),
            _ => None,
        }
    }
}

/// A batch's flags, stamped in each of its records and entries.
pub(crate) const FLAG_ORDERED: u8 = 1;

/// One persist record (CLSTORE's identifier and persist record in one): far
/// from its entry, it says the entry was written and which one it was.
///
/// ```text
///  0 u8  kind          4 u32 len        16 u64 position     28 u32 entry_crc
///  1 u8  flags         8 u64 batch      24 u32 offset       32 [24] id
///  2 u16 index                                              56 u16 count
///                                                           60 u32 crc
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub kind: Kind,
    pub flags: u8,
    /// Index of this record within its batch.
    pub index: u16,
    /// Records in its batch.
    pub count: u16,
    pub batch: u64,
    pub position: u64,
    pub id: Id,
    /// Where the entry starts, from the start of the segment file.
    pub offset: u32,
    /// Payload length.
    pub len: u32,
    /// The entry header's CRC: the entry this record names, exactly.
    pub entry_crc: u32,
}

/// How a 64-byte slot of the persist log reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    /// All zeros: never written (segments are zero-filled at creation).
    Zero,
    /// Anything else that fails its checks.
    Damaged,
    Valid(Record),
}

impl Record {
    pub fn encode(&self, out: &mut [u8]) {
        let out = &mut out[..RECORD_SIZE];
        out.fill(0);
        out[0] = self.kind as u8;
        out[1] = self.flags;
        put_u16(out, 2, self.index);
        put_u32(out, 4, self.len);
        put_u64(out, 8, self.batch);
        put_u64(out, 16, self.position);
        put_u32(out, 24, self.offset);
        put_u32(out, 28, self.entry_crc);
        out[32..56].copy_from_slice(&self.id);
        put_u16(out, 56, self.count);
        put_u32(out, 60, crc(&out[..60]));
        check_round_trip(Self::decode(out) == Slot::Valid(*self));
    }

    pub fn decode(bytes: &[u8]) -> Slot {
        let bytes = &bytes[..RECORD_SIZE];
        if bytes.iter().all(|&b| b == 0) {
            return Slot::Zero;
        }
        if get_u32(bytes, 60) != crc(&bytes[..60]) {
            return Slot::Damaged;
        }
        let Some(kind) = Kind::from_byte(bytes[0]) else {
            return Slot::Damaged;
        };
        let record = Record {
            kind,
            flags: bytes[1],
            index: get_u16(bytes, 2),
            len: get_u32(bytes, 4),
            batch: get_u64(bytes, 8),
            position: get_u64(bytes, 16),
            offset: get_u32(bytes, 24),
            entry_crc: get_u32(bytes, 28),
            id: bytes[32..56].try_into().expect("24 bytes"),
            count: get_u16(bytes, 56),
        };
        if record.index >= record.count {
            return Slot::Damaged;
        }
        Slot::Valid(record)
    }

    /// The end of a tombstone's range, which a clear carries in its id.
    pub fn clear_end(&self) -> u64 {
        get_u64(&self.id, 0)
    }

    /// The byte offset, from the start of the batch's persist blocks, where
    /// record `index` lives.
    pub fn slot_offset(index: u16) -> u64 {
        u64::from(index) * RECORD_U64
    }

    /// Persist blocks a batch of `count` records occupies.
    pub fn blocks_for(count: usize) -> u64 {
        align_up(u64_of(count) * RECORD_U64, BLOCK_U64) / BLOCK_U64
    }
}

/// An id carrying a tombstone's end.
pub(crate) fn clear_id(end: u64) -> Id {
    let mut id = [0; ID_SIZE];
    id[..8].copy_from_slice(&end.to_le_bytes());
    id
}

/// The header in front of every entry's payload: the entry's own identity,
/// so a stale or misdirected entry never passes for the one a record names,
/// and an intact entry can rebuild a lost record whole.
///
/// ```text
///  0 u32 magic         8 u64 batch      24 u16 index    28 u8  kind     32 [24] id
///  4 u32 len          16 u64 position   26 u16 count    29 u8  flags    56 u32 journal tag
///                                                                       60 u32 crc (header + payload)
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EntryHeader {
    pub kind: Kind,
    pub flags: u8,
    pub index: u16,
    pub count: u16,
    pub batch: u64,
    pub position: u64,
    pub id: Id,
    pub len: u32,
    pub tag: u32,
}

impl EntryHeader {
    /// Bytes the entry takes in the entry log: header, payload, padding to 8.
    pub fn footprint(len: u32) -> u64 {
        align_up(RECORD_U64 + u64::from(len), 8)
    }

    /// Write the header and payload into `out` and return the CRC stored.
    pub fn encode(&self, payload: &[u8], out: &mut [u8]) -> u32 {
        assert!(
            u64_of(payload.len()) == u64::from(self.len),
            "an entry header states its payload's length"
        );
        out[..RECORD_SIZE].fill(0);
        put_u32(out, 0, ENTRY_MAGIC);
        put_u32(out, 4, self.len);
        put_u64(out, 8, self.batch);
        put_u64(out, 16, self.position);
        put_u16(out, 24, self.index);
        put_u16(out, 26, self.count);
        out[28] = self.kind as u8;
        out[29] = self.flags;
        out[32..56].copy_from_slice(&self.id);
        put_u32(out, 56, self.tag);
        out[RECORD_SIZE..RECORD_SIZE + payload.len()].copy_from_slice(payload);
        let sum = entry_crc(&out[..RECORD_SIZE], payload);
        put_u32(out, 60, sum);
        check_round_trip(Self::parse(&out[..RECORD_SIZE]) == Some((*self, sum)));
        sum
    }

    /// The header's fields and its stored CRC, if the header is plausible.
    /// The CRC covers the payload too, so a caller checks it with
    /// [`entry_crc`] once it has the payload.
    pub fn parse(bytes: &[u8]) -> Option<(Self, u32)> {
        if bytes.len() < RECORD_SIZE || get_u32(bytes, 0) != ENTRY_MAGIC {
            return None;
        }
        let header = Self {
            kind: Kind::from_byte(bytes[28])?,
            flags: bytes[29],
            index: get_u16(bytes, 24),
            count: get_u16(bytes, 26),
            batch: get_u64(bytes, 8),
            position: get_u64(bytes, 16),
            id: bytes[32..56].try_into().expect("24 bytes"),
            len: get_u32(bytes, 4),
            tag: get_u32(bytes, 56),
        };
        (header.index < header.count).then_some((header, get_u32(bytes, 60)))
    }
}

/// The CRC an entry carries: over its header (CRC field zeroed) and payload.
pub(crate) fn entry_crc(header: &[u8], payload: &[u8]) -> u32 {
    let mut head = [0u8; RECORD_SIZE];
    head.copy_from_slice(&header[..RECORD_SIZE]);
    put_u32(&mut head, 60, 0);
    crc32c::crc32c_append(crc(&head), payload)
}

/// Batch numbers carry the open generation that wrote them in their high
/// bits, so a number is never reused across opens.
pub(crate) const GENERATION_SHIFT: u32 = 40;

/// The open generation a batch number was written in.
pub(crate) fn generation_of(batch: u64) -> u64 {
    batch >> GENERATION_SHIFT
}

/// The first batch number of open generation `generation`.
pub(crate) fn first_batch_of(generation: u64) -> u64 {
    (generation << GENERATION_SHIFT) + 1
}

/// One copy of the metainfo: the journal's own fields and the caller's bytes.
///
/// ```text
///  0 u32 magic   6 u8 copy      24 u64 seq     40 u64 last_batch   64 u64 open_last
///  4 u16 version 8 u128 journal 32 u64 floor   48 geometry          72 u32 open_generation
///                                                                   76 u32 len, 80 bytes..
///                                                                 4092 u32 crc
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    /// Bumped by every metainfo write; the higher of two valid copies wins.
    pub seq: u64,
    pub journal: JournalId,
    /// Positions below this are gone.
    pub floor: u64,
    /// The highest batch durable before this copy was written: a log that
    /// ends below it lost its tail.
    pub last_batch: u64,
    /// The generation of the last open, which every open bumps durably
    /// before it writes anything.
    pub open_generation: u32,
    /// The last batch that open found. A record of an older generation
    /// numbered above it was invisible to that open (torn, or a latent
    /// sector) and the log moved on: it is garbage if it ever reads back.
    pub open_last: u64,
    pub geometry: Geometry,
    pub bytes: Vec<u8>,
}

impl Meta {
    pub fn encode(&self, copy: u8, block: &mut [u8]) {
        assert!(self.bytes.len() <= META_MAX, "metainfo fits one block");
        block[..BLOCK].fill(0);
        put_u32(block, 0, META_MAGIC);
        put_u16(block, 4, VERSION);
        block[6] = copy;
        put_u128(block, 8, self.journal.0);
        put_u64(block, 24, self.seq);
        put_u64(block, 32, self.floor);
        put_u64(block, 40, self.last_batch);
        put_u32(block, 48, self.geometry.persist_blocks);
        put_u32(block, 52, self.geometry.gap_blocks);
        put_u32(block, 56, self.geometry.entry_blocks);
        put_u64(block, 64, self.open_last);
        put_u32(block, 72, self.open_generation);
        put_u32(
            block,
            76,
            u32::try_from(self.bytes.len()).expect("bounded by META_MAX"),
        );
        block[META_BYTES_AT..META_BYTES_AT + self.bytes.len()].copy_from_slice(&self.bytes);
        put_u32(block, META_CRC_AT, crc(&block[..META_CRC_AT]));
        check_round_trip(Self::decode(block, copy).as_ref() == Some(self));
    }

    /// The copy in `block`, if it checks out and is the copy it claims to be.
    pub fn decode(block: &[u8], copy: u8) -> Option<Self> {
        if get_u32(block, 0) != META_MAGIC
            || get_u16(block, 4) != VERSION
            || block[6] != copy
            || get_u32(block, META_CRC_AT) != crc(&block[..META_CRC_AT])
        {
            return None;
        }
        let len = usize::try_from(get_u32(block, 76)).ok()?;
        if len > META_MAX {
            return None;
        }
        Some(Self {
            journal: JournalId(get_u128(block, 8)),
            seq: get_u64(block, 24),
            floor: get_u64(block, 32),
            last_batch: get_u64(block, 40),
            open_last: get_u64(block, 64),
            open_generation: get_u32(block, 72),
            geometry: Geometry {
                persist_blocks: get_u32(block, 48),
                gap_blocks: get_u32(block, 52),
                entry_blocks: get_u32(block, 56),
            },
            bytes: block[META_BYTES_AT..META_BYTES_AT + len].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record {
        Record {
            kind: Kind::Put,
            flags: FLAG_ORDERED,
            index: 3,
            count: 9,
            batch: 42,
            position: 7,
            id: [5; ID_SIZE],
            offset: 8192,
            len: 100,
            entry_crc: 0xDEAD_BEEF,
        }
    }

    #[test]
    fn a_record_round_trips_and_a_flipped_bit_is_damage() {
        let mut buf = [0u8; RECORD_SIZE];
        record().encode(&mut buf);
        assert_eq!(Record::decode(&buf), Slot::Valid(record()));
        buf[17] ^= 1;
        assert_eq!(Record::decode(&buf), Slot::Damaged);
        assert_eq!(Record::decode(&[0; RECORD_SIZE]), Slot::Zero);
    }

    #[test]
    fn an_entry_round_trips_and_its_crc_covers_the_payload() {
        let header = EntryHeader {
            kind: Kind::Put,
            flags: 0,
            index: 0,
            count: 1,
            batch: 1,
            position: 9,
            id: [1; ID_SIZE],
            len: 5,
            tag: 77,
        };
        let mut out = vec![0u8; 128];
        let sum = header.encode(b"hello", &mut out);
        let (parsed, stored) = EntryHeader::parse(&out).expect("parses");
        assert_eq!((parsed, stored), (header, sum));
        assert_eq!(entry_crc(&out[..RECORD_SIZE], b"hello"), sum);
        assert_ne!(entry_crc(&out[..RECORD_SIZE], b"hellp"), sum);
    }

    #[test]
    fn a_meta_copy_is_bound_to_its_place() {
        let meta = Meta {
            seq: 3,
            journal: JournalId(9),
            floor: 4,
            last_batch: 2,
            open_generation: 3,
            open_last: 2,
            geometry: Geometry::small(),
            bytes: b"state".to_vec(),
        };
        let mut block = vec![0u8; BLOCK];
        meta.encode(1, &mut block);
        assert_eq!(Meta::decode(&block, 1), Some(meta));
        assert_eq!(Meta::decode(&block, 0), None, "copy B's bytes in A's place");
    }

    #[test]
    fn shipped_geometries_are_valid() {
        assert!(Geometry::default().is_valid());
        assert!(Geometry::small().is_valid());
    }
}
