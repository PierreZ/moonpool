//! A batch: what one commit makes durable together.

use crate::format::{BLOCK_U64, EntryHeader, Geometry, Id, MAX_BATCH_RECORDS, Record, align_up};

/// One write a batch carries, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Write {
    Put {
        position: u64,
        id: Id,
        payload: Vec<u8>,
    },
    Clear {
        start: u64,
        end: u64,
    },
}

/// The writes one [`commit`](crate::Journal::commit) makes durable: entries
/// at any position at or above the floor, tombstones, a new floor and new
/// metainfo. Owned, so a caller can stage it across calls.
///
/// Writes apply in the order they were staged; a later write to a position
/// replaces an earlier one, in this batch or a previous one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Batch {
    pub(crate) writes: Vec<Write>,
    pub(crate) floor: Option<u64>,
    pub(crate) meta: Option<Vec<u8>>,
}

impl Batch {
    /// An empty batch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Write `payload` at `position`, identified by `id`. Any position at
    /// or above the floor, in any order, with gaps: a lagging replica fills
    /// old positions while new ones arrive. Replaces what the position held.
    pub fn put(&mut self, position: u64, id: Id, payload: impl Into<Vec<u8>>) -> &mut Self {
        self.writes.push(Write::Put {
            position,
            id,
            payload: payload.into(),
        });
        self
    }

    /// Remove every entry in `start..end` (a Raft suffix truncation, or a
    /// caller discarding an ambiguous entry). An empty range does nothing.
    pub fn clear(&mut self, range: std::ops::Range<u64>) -> &mut Self {
        if !range.is_empty() {
            self.writes.push(Write::Clear {
                start: range.start,
                end: range.end,
            });
        }
        self
    }

    /// Raise the floor to `floor`: every position below it is dropped, and
    /// the segments holding only dropped entries are deleted. Never lowers
    /// it. Lands atomically with this batch's metainfo.
    pub fn truncate_prefix(&mut self, floor: u64) -> &mut Self {
        self.floor = Some(self.floor.map_or(floor, |f| f.max(floor)));
        self
    }

    /// Replace the caller's metainfo (at most [`META_MAX`](crate::META_MAX)
    /// bytes): what no peer can restore, kept in two local copies.
    ///
    /// The metainfo becomes durable only after the rest of this batch: a
    /// crash may leave the batch without it, never it without the batch. A
    /// value that depends on this batch's writes (a chosen index, a
    /// watermark whose clears ride along) can share its commit.
    pub fn set_meta(&mut self, meta: impl Into<Vec<u8>>) -> &mut Self {
        self.meta = Some(meta.into());
        self
    }

    /// Whether the batch fits one empty segment of `geometry`, the bound
    /// [`commit`](crate::Journal::commit) refuses with
    /// [`CommitError::BatchTooLarge`](crate::CommitError::BatchTooLarge): at
    /// most [`MAX_BATCH_RECORDS`] writes, their records within the persist
    /// log and their entries (header and payload, each padded to 8 bytes,
    /// the batch to a block) within the entry log. A caller with more to
    /// make durable than one segment holds splits it across commits.
    #[must_use]
    pub fn fits(&self, geometry: Geometry) -> bool {
        self.fits_with(geometry, None)
    }

    /// Whether the batch still [`fits`](Self::fits) `geometry` with one more
    /// [`put`](Self::put) of a `payload_len`-byte payload: how a caller packs
    /// its writes into commits that each fit.
    #[must_use]
    pub fn fits_another(&self, geometry: Geometry, payload_len: usize) -> bool {
        self.fits_with(geometry, Some(payload_len))
    }

    fn fits_with(&self, geometry: Geometry, another: Option<usize>) -> bool {
        let count = self.writes.len() + usize::from(another.is_some());
        if count > MAX_BATCH_RECORDS {
            return false;
        }
        let lens = self
            .writes
            .iter()
            .map(|write| match write {
                Write::Put { payload, .. } => payload.len(),
                Write::Clear { .. } => 0,
            })
            .chain(another);
        let mut entry_bytes = 0_u64;
        for len in lens {
            let Ok(len) = u32::try_from(len) else {
                return false;
            };
            entry_bytes += EntryHeader::footprint(len);
        }
        Record::blocks_for(count) <= u64::from(geometry.persist_blocks)
            && align_up(entry_bytes, BLOCK_U64) <= u64::from(geometry.entry_blocks) * BLOCK_U64
    }

    /// Whether the batch would write nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.floor.is_none() && self.meta.is_none()
    }
}
