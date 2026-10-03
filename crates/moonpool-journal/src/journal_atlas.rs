//! Where a journal's bytes live: every region recovery reads, named by what
//! it holds and located by file and byte range.
//!
//! A fault injector that knows the layout can aim where recovery has to make
//! a decision — a live entry, the slot that identifies it, the last batch, a
//! header or metadata copy — instead of spreading damage uniformly over a
//! file that is mostly preallocated zeros. And because every region carries
//! its meaning, the injector can tell what the damage it caused *should*
//! make recovery do: [`JournalAtlas::regions_in`] maps damaged bytes back to the
//! regions they touch.
//!
//! Two ways to get one:
//!
//! - [`Journal::atlas`] charts an open journal from what its recovery scan
//!   verified — a snapshot; take a new one after the journal changes.
//! - [`JournalAtlas::scan`] charts a closed journal directory from its slot tables
//!   alone, without opening it and so without repairing anything: what an
//!   injector wants before a boot, to damage the bytes the next open will
//!   read.
//!
//! [`Journal::atlas`]: crate::Journal::atlas

use std::io;
use std::ops::Range;

use moonpool_core::{LayoutRegion, OpenOptions, StorageFile, StorageProvider};

use crate::JournalError;
use crate::layout::{
    BLOCK_U64, ENTRY_HEADER_SIZE, Geometry, SLOT_SIZE, SLOT_TABLE_OFFSET, Slot, SlotState,
};
use crate::segment::{EntryId, parse_segment_name, segment_path};

/// Bytes of a segment header copy that recovery checks: the fields and their
/// CRC (`layout::Header`).
const HEADER_CHECKED: u64 = 28;

/// Bytes of a metadata copy's own header, before its payload (`dual.rs`).
const META_HEADER: u64 = 24;

/// What one region of the journal holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JournalRegion {
    /// One of the two header copies of the segment whose first index is
    /// `segment`. Recovery repairs one copy from its twin and refuses to
    /// open the segment when both are damaged.
    Header {
        /// The segment's first index.
        segment: u64,
        /// Which copy: 0 (block 0) or 1 (block 1).
        copy: u8,
    },
    /// The slot — the far identifier — of an entry. Damaged beside an intact
    /// entry, it is rebuilt; beside a damaged entry, recovery refuses to
    /// start (a double fault).
    Slot(EntryId),
    /// An entry's header and payload. Damaged beside an intact slot, it is
    /// reported corrupt (or ambiguous, in the last batch).
    Entry(EntryId),
    /// One copy of the caller's metadata (`meta.0` / `meta.1`). One damaged
    /// copy is repaired from the other; both damaged is
    /// [`JournalError::MetadataCorrupt`](crate::JournalError::MetadataCorrupt).
    Meta {
        /// Which copy: 0 or 1.
        copy: u8,
    },
}

impl JournalRegion {
    /// The kind label of a header copy in a [`LayoutRegion`].
    pub const HEADER: &'static str = "journal header";
    /// The kind label of a slot (a far identifier).
    pub const SLOT: &'static str = "journal slot";
    /// The kind label of an entry's header and payload.
    pub const ENTRY: &'static str = "journal entry";
    /// The kind label of a metadata copy.
    pub const META: &'static str = "journal meta";

    /// This region's kind, as its [`LayoutRegion`] labels it.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Header { .. } => Self::HEADER,
            Self::Slot(_) => Self::SLOT,
            Self::Entry(_) => Self::ENTRY,
            Self::Meta { .. } => Self::META,
        }
    }
}

/// A journal region and where it lives, in the format-neutral form a
/// simulator aims faults with.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChartedRegion {
    /// What the bytes hold, in the journal's terms.
    pub region: JournalRegion,
    /// Where they are: file, byte range, and [`JournalRegion::kind`].
    pub layout: LayoutRegion,
}

/// Every region recovery reads, as of one moment of a journal's life.
///
/// Regions are listed metadata first, then segment by segment: the two
/// header copies, then each live entry's slot and entry in index order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalAtlas {
    regions: Vec<ChartedRegion>,
    last_batch: Range<u64>,
    live: Range<u64>,
    /// Charted indexes whose entry opens an append batch, ascending.
    batch_starts: Vec<u64>,
}

impl JournalAtlas {
    /// Every region, in the order described on [`JournalAtlas`].
    #[must_use]
    pub fn regions(&self) -> &[ChartedRegion] {
        &self.regions
    }

    /// Every region in the format-neutral form: what a simulator's fault
    /// focus takes (`FaultFocus::layout` in `moonpool-sim`), weighing each by
    /// its [`kind`](JournalRegion::kind).
    pub fn layout(&self) -> impl Iterator<Item = &LayoutRegion> {
        self.regions.iter().map(|charted| &charted.layout)
    }

    /// The live indexes: from the journal's first index to its next one.
    #[must_use]
    pub fn live(&self) -> Range<u64> {
        self.live.clone()
    }

    /// The indexes of the log's last append batch — the entries a crash may
    /// leave damaged beside intact identifiers, whose damage recovery
    /// reports as ambiguous rather than corrupt. Empty when the log is.
    #[must_use]
    pub fn last_batch(&self) -> Range<u64> {
        self.last_batch.clone()
    }

    /// Whether the charted entry at `index` opens an append batch.
    #[must_use]
    pub fn starts_batch(&self, index: u64) -> bool {
        self.batch_starts.binary_search(&index).is_ok()
    }

    /// Where `region` lives, if the journal holds it.
    #[must_use]
    pub fn locate(&self, region: JournalRegion) -> Option<&LayoutRegion> {
        self.regions
            .iter()
            .find(|located| located.region == region)
            .map(|located| &located.layout)
    }

    /// Where the entry at `index` lives, if it is live.
    #[must_use]
    pub fn entry(&self, index: u64) -> Option<&ChartedRegion> {
        self.regions
            .iter()
            .find(|located| matches!(located.region, JournalRegion::Entry(id) if id.index == index))
    }

    /// Where the slot of the entry at `index` lives, if it is live.
    #[must_use]
    pub fn slot(&self, index: u64) -> Option<&ChartedRegion> {
        self.regions
            .iter()
            .find(|located| matches!(located.region, JournalRegion::Slot(id) if id.index == index))
    }

    /// Every region sharing a byte with `bytes` of the file at `path`: what
    /// damage to those bytes reaches. A sector of the slot table holds
    /// several slots, and a block several entries, so one damaged sector can
    /// reach more than one region.
    #[must_use]
    pub fn regions_in(&self, path: &str, bytes: &Range<u64>) -> Vec<JournalRegion> {
        self.regions
            .iter()
            .filter(|located| located.layout.overlaps(path, bytes))
            .map(|located| located.region)
            .collect()
    }

    /// Chart the closed journal under `dir`, laid out with `geometry`, from
    /// its files alone: the metadata copies that exist, each segment's
    /// header copies, and every entry a valid slot locates. Nothing is
    /// opened as a journal, so nothing is repaired.
    ///
    /// The slot table is the map, so the scan sees what the next open's
    /// identifiers say: an entry whose slot is already damaged or empty is
    /// not charted (recovery would locate it from its predecessor instead),
    /// and each segment's walk stops at its first empty slot. The last batch
    /// is read from the slots' batch-start flags.
    ///
    /// # Errors
    ///
    /// [`JournalError::Io`] if the directory cannot be listed or a file
    /// read fails.
    pub async fn scan<P: StorageProvider>(
        provider: &P,
        dir: &str,
        geometry: Geometry,
    ) -> Result<Self, JournalError> {
        let mut atlas = Self::default();
        let mut names = provider.list_dir(dir).await?;
        names.sort_unstable();
        for copy in 0..2u8 {
            let name = format!("meta.{copy}");
            if names.contains(&name) {
                let path = format!("{dir}/{name}");
                let file = provider.open(&path, OpenOptions::read_only()).await?;
                let mut header = [0u8; 24];
                let read = read_full(&file, 0, &mut header).await?;
                let size = file.size().await?;
                let len = if read == header.len() {
                    u64::from(u32::from_le_bytes([
                        header[16], header[17], header[18], header[19],
                    ]))
                } else {
                    0
                };
                let payload = len.min(size.saturating_sub(META_HEADER));
                atlas.push_meta(&path, copy, payload);
            }
        }
        let mut firsts: Vec<u64> = names.iter().filter_map(|n| parse_segment_name(n)).collect();
        firsts.sort_unstable();
        let mut live: Option<Range<u64>> = None;
        // Indexes and batch flags of the last segment holding entries.
        let mut tail: Vec<(u64, bool)> = Vec::new();
        for first in firsts {
            let path = segment_path(dir, first);
            atlas.push_headers(&path, first);
            let file = provider.open(&path, OpenOptions::read_only()).await?;
            let table_len = geometry.slot_table_end() - SLOT_TABLE_OFFSET;
            let table_len = usize::try_from(table_len).map_err(|_| {
                JournalError::InvalidConfig("slot table overflows usize".to_string())
            })?;
            let mut table = vec![0u8; table_len];
            let read = read_full(&file, SLOT_TABLE_OFFSET, &mut table).await?;
            let mut flags = Vec::new();
            for (rel, bytes) in (0u64..).zip(table[..read].chunks_exact(SLOT_SIZE)) {
                let index = first + rel;
                match Slot::decode(bytes, index) {
                    SlotState::Empty => break,
                    SlotState::Bad => {}
                    SlotState::Valid(slot) => {
                        atlas.push_entry(
                            &path,
                            slot.id(),
                            rel,
                            (u64::from(slot.offset), slot.length),
                            slot.batch_start,
                        );
                        flags.push((index, slot.batch_start));
                        let range = live.get_or_insert(index..index);
                        range.end = index + 1;
                    }
                }
            }
            if !flags.is_empty() {
                tail = flags;
            }
        }
        let last_batch = tail
            .iter()
            .rposition(|(_, start)| *start)
            .or_else(|| tail.len().checked_sub(1))
            .map_or(0..0, |from| tail[from].0..tail[tail.len() - 1].0 + 1);
        atlas.set_live(live.unwrap_or(0..0), last_batch);
        Ok(atlas)
    }

    pub(crate) fn push(&mut self, region: JournalRegion, path: &str, offset: u64, len: u64) {
        self.regions.push(ChartedRegion {
            region,
            layout: LayoutRegion {
                path: path.to_string(),
                bytes: offset..offset + len,
                kind: region.kind(),
                // The journal does not know what its caller replicates: the
                // caller maps tags to stripes (`LayoutRegion::striped`).
                stripe: None,
            },
        });
    }

    pub(crate) fn set_live(&mut self, live: Range<u64>, last_batch: Range<u64>) {
        self.live = live;
        self.last_batch = last_batch;
    }

    /// Add a metadata copy holding a `payload_len`-byte value.
    pub(crate) fn push_meta(&mut self, path: &str, copy: u8, payload_len: u64) {
        self.push(
            JournalRegion::Meta { copy },
            path,
            0,
            META_HEADER + payload_len,
        );
    }

    /// Add a segment's two header copies.
    pub(crate) fn push_headers(&mut self, path: &str, segment: u64) {
        for copy in 0..2u8 {
            self.push(
                JournalRegion::Header { segment, copy },
                path,
                u64::from(copy) * BLOCK_U64,
                HEADER_CHECKED,
            );
        }
    }

    /// Add one live entry and its slot. `rel` is its position in its
    /// segment, `(offset, length)` the entry's offset and payload length.
    pub(crate) fn push_entry(
        &mut self,
        path: &str,
        id: EntryId,
        rel: u64,
        (offset, length): (u64, u32),
        batch_start: bool,
    ) {
        if batch_start {
            self.batch_starts.push(id.index);
        }
        let slot_size = SLOT_SIZE as u64;
        self.push(
            JournalRegion::Slot(id),
            path,
            SLOT_TABLE_OFFSET + rel * slot_size,
            slot_size,
        );
        self.push(
            JournalRegion::Entry(id),
            path,
            offset,
            ENTRY_HEADER_SIZE as u64 + u64::from(length),
        );
    }
}

/// Read into all of `buf` from `offset`, stopping early only at the end of
/// the file. Returns the bytes read.
async fn read_full<F: StorageFile>(file: &F, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        let read = file
            .read_at(
                offset + u64::try_from(done).unwrap_or(u64::MAX),
                &mut buf[done..],
            )
            .await?;
        if read == 0 {
            break;
        }
        done += read;
    }
    Ok(done)
}
