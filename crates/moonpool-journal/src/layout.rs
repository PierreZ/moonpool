//! On-disk formats: the segment geometry, the header, the slot, and the entry.
//!
//! ```text
//! Offset      Region        Contents
//! 0           Header A      magic, version, first_index,
//! BLOCK       Header B      slot_count, data_start, crc
//! 2 × BLOCK   Slot table    slot_count × 64 B
//! …           Guard gap     zeros (keeps identifiers away from entries)
//! data_start  Data region   append-only entries → end of file
//! ```
//!
//! Every integer is little-endian. Nothing here does I/O: these are the
//! encoders, the decoders, and the arithmetic the segment and the recovery
//! scan share.

use crate::JournalError;

/// The journal's block: the unit `BlockFile` transfers are made of. It is not
/// a layout unit — entries are packed contiguously, 8-byte aligned — only the
/// granularity of I/O.
pub const BLOCK: usize = 4096;

/// [`BLOCK`] as a file offset.
pub(crate) const BLOCK_U64: u64 = BLOCK as u64;

/// Byte offset of the slot table: after the two header copies.
pub(crate) const SLOT_TABLE_OFFSET: u64 = 2 * BLOCK_U64;

/// Size of one slot. A power of two, so a slot never straddles a sector.
pub const SLOT_SIZE: usize = 64;

/// Size of an entry's header; the payload follows it.
pub const ENTRY_HEADER_SIZE: usize = 64;

/// Size of the caller's identity [`Tag`].
pub const TAG_SIZE: usize = 24;

/// Opaque caller identity stored with an entry, in its slot and in its header.
///
/// The slot's `index` and `epoch` are Raft's identity. A caller whose
/// identity is wider — a Paxos acceptor journaling `(slot, ballot)` records
/// under a write-ahead sequence number — puts the rest here: the slot keeps
/// it far from the entry, so it survives when the entry's bytes do not, and
/// recovery reports it with every corrupt entry.
pub type Tag = [u8; TAG_SIZE];

/// Where the tag sits in a slot and in an entry header.
const TAG_AT: usize = 32;

/// Where the flags word sits in a slot and in an entry header.
const FLAGS_AT: usize = 28;

/// Flag: this entry is the first of an append batch — of the entries one
/// `fdatasync` made durable together.
const FLAG_BATCH_START: u32 = 1;

/// Flag, beside the batch start: this batch is a checkpoint, which once
/// durable supersedes every entry before it. The field at [`COUNT_AT`] then
/// holds how many entries the batch has, so a checkpoint a crash cut short
/// can never pass for a complete one.
const FLAG_CHECKPOINT: u32 = 1 << 1;

/// Where a checkpoint's entry count sits, in a slot and in an entry header
/// (the first four bytes of their reserved field).
const COUNT_AT: usize = 56;

/// Flag, on a slot only: a reserved record, the formatted content of a slot
/// that holds no entry. Its index is the slot's own, every other field zero.
const FLAG_RESERVED: u32 = 1 << 2;

/// Flag, on a slot only: the entries after this one were cut (by a suffix
/// truncation, or by recovery discarding a torn tail), so no entry continues
/// this one's batch. Whatever bytes follow it in its block are discarded.
const FLAG_CUT: u32 = 1 << 3;

const HEADER_MAGIC: u32 = u32::from_le_bytes(*b"MPJH");
const ENTRY_MAGIC: u32 = u32::from_le_bytes(*b"MPJE");
/// Version 3 added the batch-start flag in the slot's and the entry's flags
/// word (reserved, always zero, in version 2), so a version-2 segment would
/// read as one never-ending batch and is refused instead.
///
/// Version 4 starts every batch on a fresh block, and recovery rejects a
/// batch start anywhere else: a version-3 segment, whose batches are packed
/// back to back, is refused rather than misread. It also formats every slot
/// with a reserved record, so all zeros in a slot is damage, never "empty",
/// where version 3 read zeros as never written; and marks a checkpoint
/// batch, with its entry count, in its first entry and slot.
const FORMAT_VERSION: u32 = 4;

/// Bytes of the header block covered by its CRC.
const HEADER_LEN: usize = 24;

/// The shape of every segment in one journal.
///
/// The defaults are the CLSTORE layout: 64 MiB segments, 32,768 slots (a
/// 2 MiB slot table of 64-byte slots), and a data region starting at 4 MiB,
/// which leaves a guard gap of about 2 MiB between the last identifier and
/// the first entry.
/// Shrink them in tests to make rollover cheap to reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// Slots per segment: the most entries one segment can hold.
    pub slot_count: u32,
    /// Byte offset of the data region. Must be block-aligned, at or past the
    /// end of the slot table, and below 4 GiB (the header stores it in 32
    /// bits).
    pub data_start: u64,
    /// Size of every segment file, preallocated with zeros. Must be
    /// block-aligned, larger than `data_start`, and at most 4 GiB (slot
    /// offsets are 32-bit). Not recorded in the header: a file of any other
    /// size is a metadata fault.
    pub segment_size: u64,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            slot_count: 32_768,
            data_start: 4 << 20,
            segment_size: 64 << 20,
        }
    }
}

impl Geometry {
    /// Check the geometry is one a segment can be laid out with.
    ///
    /// # Errors
    ///
    /// [`JournalError::InvalidConfig`] naming the violated rule.
    pub fn validate(&self) -> Result<(), JournalError> {
        let invalid = |why: &str| Err(JournalError::InvalidConfig(why.to_string()));
        if self.slot_count == 0 {
            return invalid("slot_count must be positive");
        }
        if !self.data_start.is_multiple_of(BLOCK_U64) {
            return invalid("data_start must be block-aligned");
        }
        if self.data_start < self.slot_table_end() {
            return invalid("data_start must not overlap the slot table");
        }
        if !self.segment_size.is_multiple_of(BLOCK_U64) {
            return invalid("segment_size must be block-aligned");
        }
        if self.segment_size <= self.data_start {
            return invalid("segment_size must leave room for a data region");
        }
        if self.data_start > u64::from(u32::MAX) {
            return invalid("data_start must fit 32 bits");
        }
        if self.segment_size > u64::from(u32::MAX) + 1 {
            return invalid("segment_size must fit 32-bit slot offsets");
        }
        Ok(())
    }

    /// First byte past the slot table.
    pub(crate) fn slot_table_end(&self) -> u64 {
        SLOT_TABLE_OFFSET + u64::from(self.slot_count) * SLOT_SIZE as u64
    }

    /// Blocks the slot table spans.
    pub(crate) fn slot_table_blocks(&self) -> u64 {
        (self.slot_table_end() - SLOT_TABLE_OFFSET).div_ceil(BLOCK_U64)
    }

    /// Bytes available to entries.
    pub(crate) fn data_capacity(&self) -> u64 {
        self.segment_size - self.data_start
    }
}

/// Round `value` up to a multiple of `to` (a power of two).
pub(crate) fn align_up(value: u64, to: u64) -> u64 {
    debug_assert!(to.is_power_of_two());
    (value + to - 1) & !(to - 1)
}

/// Round `value` down to a multiple of `to` (a power of two).
pub(crate) fn align_down(value: u64, to: u64) -> u64 {
    debug_assert!(to.is_power_of_two());
    value & !(to - 1)
}

/// On-disk size of an entry with a `payload_len`-byte payload: header plus
/// payload, padded to 8 bytes.
pub(crate) fn entry_size(payload_len: u32) -> u64 {
    align_up(ENTRY_HEADER_SIZE as u64 + u64::from(payload_len), 8)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4-byte field"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8-byte field"))
}

fn flags(batch_start: bool, checkpoint: Option<u32>) -> u32 {
    let start = if batch_start { FLAG_BATCH_START } else { 0 };
    start | checkpoint.map_or(0, |_| FLAG_CHECKPOINT)
}

/// The checkpoint entry count the flags and count field record, if any.
fn checkpoint_at(bytes: &[u8]) -> Option<u32> {
    (u32_at(bytes, FLAGS_AT) & FLAG_CHECKPOINT != 0).then(|| u32_at(bytes, COUNT_AT))
}

fn tag_at(bytes: &[u8]) -> Tag {
    bytes[TAG_AT..TAG_AT + TAG_SIZE]
        .try_into()
        .expect("tag-sized field")
}

/// A segment's header, written once when the segment is created, in two
/// copies (blocks 0 and 1): magic, version, first index, slot count, data
/// start, and a CRC over them.
///
/// ```text
/// 0  u32 magic   4  u32 version   8  u64 first_index
/// 16 u32 slot_count   20 u32 data_start   24 u32 crc (0..24)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    pub first_index: u64,
    pub slot_count: u32,
    pub data_start: u64,
}

impl Header {
    /// The header a segment of `geometry` starting at `first_index` carries.
    pub fn new(first_index: u64, geometry: Geometry) -> Self {
        Self {
            first_index,
            slot_count: geometry.slot_count,
            data_start: geometry.data_start,
        }
    }

    /// Encode into the start of a header block; the rest stays zero.
    pub fn encode(&self, block: &mut [u8]) {
        block[0..4].copy_from_slice(&HEADER_MAGIC.to_le_bytes());
        block[4..8].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        block[8..16].copy_from_slice(&self.first_index.to_le_bytes());
        block[16..20].copy_from_slice(&self.slot_count.to_le_bytes());
        let data_start = u32::try_from(self.data_start).expect("validated to fit 32 bits");
        block[20..24].copy_from_slice(&data_start.to_le_bytes());
        let crc = crc32c::crc32c(&block[..HEADER_LEN]);
        block[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&crc.to_le_bytes());
    }

    /// Decode one header copy, or `None` if it does not check out.
    pub fn decode(block: &[u8]) -> Option<Self> {
        if u32_at(block, 0) != HEADER_MAGIC || u32_at(block, 4) != FORMAT_VERSION {
            return None;
        }
        if crc32c::crc32c(&block[..HEADER_LEN]) != u32_at(block, HEADER_LEN) {
            return None;
        }
        Some(Self {
            first_index: u64_at(block, 8),
            slot_count: u32_at(block, 16),
            data_start: u64::from(u32_at(block, 20)),
        })
    }
}

/// One slot: an entry's identifier, stored far from the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    pub index: u64,
    pub epoch: u64,
    /// Byte offset of the entry within the segment file.
    pub offset: u32,
    /// Payload length.
    pub length: u32,
    /// The entry's own CRC, repeated.
    pub entry_crc: u32,
    /// The caller's identity, repeated from the entry.
    pub tag: Tag,
    /// The entry opened an append batch, repeated from the entry.
    pub batch_start: bool,
    /// The entries after this one were cut: none continues its batch. In
    /// the slot alone — the entry was written before the cut.
    pub cut: bool,
    /// This entry opens a checkpoint batch of this many entries, repeated
    /// from the entry.
    pub checkpoint: Option<u32>,
}

impl Slot {
    /// The identity this slot records for its entry.
    pub fn id(&self) -> crate::EntryId {
        crate::EntryId {
            index: self.index,
            epoch: self.epoch,
            tag: self.tag,
        }
    }
}

/// What a slot's bytes turned out to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotState {
    /// A reserved record naming the index it sits at: formatted and never
    /// written since, or reset by a truncation. A positive statement that
    /// no entry is here.
    Reserved,
    /// Its CRC passes and it names the index it sits at.
    Valid(Slot),
    /// Anything else — torn, rotted, zeroed, stale, or a reserved record
    /// naming another index (misdirected).
    Bad,
}

/// Bytes of a slot covered by its CRC; the CRC follows.
const SLOT_CRC_AT: usize = SLOT_SIZE - 4;

impl Slot {
    /// Encode into `out` (exactly [`SLOT_SIZE`] bytes):
    ///
    /// ```text
    /// 0  u64 index       16 u32 offset      24 u32 entry_crc   32 tag (24 B)
    /// 8  u64 epoch       20 u32 length      28 u32 flags       56 u32 reserved
    ///                                                          60 u32 crc (0..60)
    /// ```
    pub fn encode(&self, out: &mut [u8]) {
        out.fill(0);
        out[0..8].copy_from_slice(&self.index.to_le_bytes());
        out[8..16].copy_from_slice(&self.epoch.to_le_bytes());
        out[16..20].copy_from_slice(&self.offset.to_le_bytes());
        out[20..24].copy_from_slice(&self.length.to_le_bytes());
        out[24..28].copy_from_slice(&self.entry_crc.to_le_bytes());
        let cut = if self.cut { FLAG_CUT } else { 0 };
        let flags = flags(self.batch_start, self.checkpoint) | cut;
        out[FLAGS_AT..FLAGS_AT + 4].copy_from_slice(&flags.to_le_bytes());
        out[TAG_AT..TAG_AT + TAG_SIZE].copy_from_slice(&self.tag);
        out[COUNT_AT..COUNT_AT + 4].copy_from_slice(&self.checkpoint.unwrap_or(0).to_le_bytes());
        let crc = crc32c::crc32c(&out[..SLOT_CRC_AT]);
        out[SLOT_CRC_AT..SLOT_SIZE].copy_from_slice(&crc.to_le_bytes());
    }

    /// Encode the reserved record of the slot for `index` into `out`
    /// (exactly [`SLOT_SIZE`] bytes): the index, the reserved flag, zeros,
    /// and the CRC — `TigerBeetle`'s reserved header, whose op is its slot.
    pub fn encode_reserved(index: u64, out: &mut [u8]) {
        out.fill(0);
        out[0..8].copy_from_slice(&index.to_le_bytes());
        out[FLAGS_AT..FLAGS_AT + 4].copy_from_slice(&FLAG_RESERVED.to_le_bytes());
        let crc = crc32c::crc32c(&out[..SLOT_CRC_AT]);
        out[SLOT_CRC_AT..SLOT_SIZE].copy_from_slice(&crc.to_le_bytes());
    }

    /// Classify the slot stored for `index`. All zeros fails the CRC like
    /// any other damage.
    pub fn decode(bytes: &[u8], index: u64) -> SlotState {
        if bytes.iter().all(|byte| *byte == 0) {
            return SlotState::Bad;
        }
        if crc32c::crc32c(&bytes[..SLOT_CRC_AT]) != u32_at(bytes, SLOT_CRC_AT) {
            return SlotState::Bad;
        }
        if u32_at(bytes, FLAGS_AT) & FLAG_RESERVED != 0 {
            let mut expected = [0u8; SLOT_SIZE];
            Self::encode_reserved(index, &mut expected);
            return if bytes == expected {
                SlotState::Reserved
            } else {
                SlotState::Bad
            };
        }
        let slot = Self {
            index: u64_at(bytes, 0),
            epoch: u64_at(bytes, 8),
            offset: u32_at(bytes, 16),
            length: u32_at(bytes, 20),
            entry_crc: u32_at(bytes, 24),
            tag: tag_at(bytes),
            batch_start: u32_at(bytes, FLAGS_AT) & FLAG_BATCH_START != 0,
            cut: u32_at(bytes, FLAGS_AT) & FLAG_CUT != 0,
            checkpoint: checkpoint_at(bytes),
        };
        if slot.index == index {
            SlotState::Valid(slot)
        } else {
            SlotState::Bad
        }
    }
}

/// The fixed part of an entry, parsed but not yet checked against its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EntryHeader {
    pub length: u32,
    pub index: u64,
    pub epoch: u64,
    pub crc: u32,
    pub tag: Tag,
    pub batch_start: bool,
    pub checkpoint: Option<u32>,
}

impl EntryHeader {
    /// Parse a header, or `None` if the magic is wrong.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if u32_at(bytes, 0) != ENTRY_MAGIC {
            return None;
        }
        Some(Self {
            length: u32_at(bytes, 4),
            index: u64_at(bytes, 8),
            epoch: u64_at(bytes, 16),
            crc: u32_at(bytes, 24),
            tag: tag_at(bytes),
            batch_start: u32_at(bytes, FLAGS_AT) & FLAG_BATCH_START != 0,
            checkpoint: checkpoint_at(bytes),
        })
    }
}

/// The CRC an entry carries: over its header (minus the CRC field itself)
/// and its payload.
fn entry_crc(header: &[u8], payload: &[u8]) -> u32 {
    let crc = crc32c::crc32c(&header[..24]);
    let crc = crc32c::crc32c_append(crc, &header[28..ENTRY_HEADER_SIZE]);
    crc32c::crc32c_append(crc, payload)
}

/// Encode an entry into `out` (exactly [`entry_size`] bytes); returns its CRC.
///
/// ```text
/// 0  u32 magic    8  u64 index    24 u32 crc        32 tag (24 B)
/// 4  u32 length   16 u64 epoch    28 u32 flags      56 u64 reserved
///                                                   64 payload …
/// ```
pub(crate) fn encode_entry(
    index: u64,
    epoch: u64,
    tag: &Tag,
    (batch_start, checkpoint): (bool, Option<u32>),
    payload: &[u8],
    out: &mut [u8],
) -> u32 {
    let length = u32::try_from(payload.len()).expect("payload length checked by the caller");
    out[..ENTRY_HEADER_SIZE].fill(0);
    out[0..4].copy_from_slice(&ENTRY_MAGIC.to_le_bytes());
    out[4..8].copy_from_slice(&length.to_le_bytes());
    out[8..16].copy_from_slice(&index.to_le_bytes());
    out[16..24].copy_from_slice(&epoch.to_le_bytes());
    // 24..28 is the CRC, filled in below; past the tag, a checkpoint's
    // entry count, then reserved.
    out[FLAGS_AT..FLAGS_AT + 4].copy_from_slice(&flags(batch_start, checkpoint).to_le_bytes());
    out[COUNT_AT..COUNT_AT + 4].copy_from_slice(&checkpoint.unwrap_or(0).to_le_bytes());
    out[TAG_AT..TAG_AT + TAG_SIZE].copy_from_slice(tag);
    out[ENTRY_HEADER_SIZE..ENTRY_HEADER_SIZE + payload.len()].copy_from_slice(payload);
    out[ENTRY_HEADER_SIZE + payload.len()..].fill(0);
    let crc = entry_crc(&out[..ENTRY_HEADER_SIZE], payload);
    out[24..28].copy_from_slice(&crc.to_le_bytes());
    crc
}

/// Whether `bytes` (header followed by at least `header.length` payload
/// bytes) is an intact entry. The caller still compares the index and epoch.
pub(crate) fn entry_crc_ok(header: &EntryHeader, bytes: &[u8]) -> bool {
    let end = ENTRY_HEADER_SIZE + header.length as usize;
    bytes.len() >= end
        && entry_crc(&bytes[..ENTRY_HEADER_SIZE], &bytes[ENTRY_HEADER_SIZE..end]) == header.crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_geometry_is_the_clstore_layout() {
        let geometry = Geometry::default();
        geometry.validate().expect("default geometry is valid");
        assert_eq!(geometry.slot_table_end(), 8192 + 2 * 1024 * 1024);
        assert!(
            BLOCK.is_multiple_of(SLOT_SIZE),
            "a slot never straddles a block"
        );
        assert!(geometry.data_start - geometry.slot_table_end() >= 2 * 1024 * 1024 - 8192);
    }

    #[test]
    fn slots_round_trip_and_reject_the_wrong_index() {
        let slot = Slot {
            index: 7,
            epoch: 3,
            offset: 4096,
            length: 10,
            entry_crc: 0xDEAD_BEEF,
            tag: [0xA5; TAG_SIZE],
            batch_start: true,
            cut: true,
            checkpoint: Some(3),
        };
        let mut bytes = [0u8; SLOT_SIZE];
        slot.encode(&mut bytes);
        assert_eq!(Slot::decode(&bytes, 7), SlotState::Valid(slot));
        assert_eq!(Slot::decode(&bytes, 8), SlotState::Bad);
        assert_eq!(
            Slot::decode(&[0; SLOT_SIZE], 7),
            SlotState::Bad,
            "zeros are damage"
        );
        bytes[3] ^= 1;
        assert_eq!(Slot::decode(&bytes, 7), SlotState::Bad);
        bytes[3] ^= 1;
        bytes[TAG_AT + 5] ^= 1;
        assert_eq!(
            Slot::decode(&bytes, 7),
            SlotState::Bad,
            "the CRC covers the tag"
        );
    }

    #[test]
    fn a_reserved_record_names_its_own_slot() {
        let mut bytes = [0u8; SLOT_SIZE];
        Slot::encode_reserved(7, &mut bytes);
        assert_eq!(Slot::decode(&bytes, 7), SlotState::Reserved);
        assert_eq!(
            Slot::decode(&bytes, 8),
            SlotState::Bad,
            "a reserved record in another slot is misdirected"
        );
        bytes[1] ^= 1;
        assert_eq!(Slot::decode(&bytes, 7), SlotState::Bad);
    }

    #[test]
    fn entries_round_trip_and_detect_a_flipped_payload_bit() {
        let payload = b"hello journal";
        let mut bytes = vec![0u8; usize::try_from(entry_size(13)).expect("small")];
        let tag = [7; TAG_SIZE];
        encode_entry(9, 2, &tag, (true, Some(2)), payload, &mut bytes);
        let header = EntryHeader::parse(&bytes).expect("magic");
        assert_eq!((header.index, header.epoch, header.length), (9, 2, 13));
        assert_eq!(header.tag, tag);
        assert!(header.batch_start);
        assert_eq!(header.checkpoint, Some(2));
        bytes[FLAGS_AT] ^= 1;
        assert!(!entry_crc_ok(&header, &bytes), "the CRC covers the flags");
        bytes[FLAGS_AT] ^= 1;
        assert!(entry_crc_ok(&header, &bytes));
        bytes[TAG_AT] ^= 1;
        assert!(!entry_crc_ok(&header, &bytes), "the CRC covers the tag");
        bytes[TAG_AT] ^= 1;
        bytes[ENTRY_HEADER_SIZE + 1] ^= 0x40;
        assert!(!entry_crc_ok(&header, &bytes));
    }

    #[test]
    fn headers_round_trip() {
        let header = Header::new(1 << 20, Geometry::default());
        let mut block = vec![0u8; BLOCK];
        header.encode(&mut block);
        assert_eq!(Header::decode(&block), Some(header));
        block[9] ^= 1;
        assert_eq!(Header::decode(&block), None);
    }
}
