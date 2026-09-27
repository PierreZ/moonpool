//! The journal: the ordered segments of a directory, and the caller's
//! metadata beside them.

use std::ops::Range;

use moonpool_core::{DirectIo, StorageProvider};
use tracing::instrument;

use crate::dual::DualFile;
use crate::layout::{ENTRY_HEADER_SIZE, Geometry, TAG_SIZE, Tag, entry_size};
use crate::segment::{Entry, Segment, is_segment_leftover, parse_segment_name, segment_path};
use crate::{EntryId, JournalError};

/// How to lay out and drive a journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalConfig {
    /// The shape of every segment. Must match what an existing journal was
    /// created with: a segment whose header or size disagrees is refused.
    pub geometry: Geometry,
    /// Direct-I/O policy for segment files.
    pub direct_io: DirectIo,
    /// The index of the first entry of a freshly created journal. Ignored
    /// when the directory already holds segments.
    pub first_index: u64,
    /// What opening does with the ambiguous last entry.
    pub ambiguous_tail: AmbiguousTail,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            geometry: Geometry::default(),
            direct_io: DirectIo::Optional,
            first_index: 1,
            ambiguous_tail: AmbiguousTail::Truncate,
        }
    }
}

/// What opening does with the log's last entry when its identifier is intact
/// but the entry is not.
///
/// A crash between the slot write and the sync leaves exactly that, and so
/// does corruption of an entry that was synced and acknowledged: no local
/// algorithm tells them apart (the CLSTORE paper's Appendix A). Either way
/// the entry is reported in [`Recovery::ambiguous_tail`]; this decides
/// whether it is also removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AmbiguousTail {
    /// Treat it as a torn write and truncate it — right for a single node,
    /// whose only other choice would be to refuse to start. A replicated
    /// caller that instead needs to find out whether it was committed must
    /// act on the report before the next crash: once truncated, the
    /// identity is gone from disk.
    #[default]
    Truncate,
    /// Keep it in the log, marked corrupt like a damaged entry mid-log: a
    /// read returns [`JournalError::Corrupt`], and its identity survives
    /// every later reopen. The caller decides — for example by asking its
    /// peers whether the entry was committed — and discards it with
    /// [`Journal::truncate_suffix`] if it was not. Appending after it
    /// leaves it an ordinary corrupt entry, reported in
    /// [`Recovery::corrupt`] from then on.
    Keep,
}

/// An entry to append: the epoch (term) it belongs to, the caller's
/// identity tag, and its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    /// The epoch this entry was written in.
    pub epoch: u64,
    /// The caller's identity for this entry, kept in its slot far from the
    /// entry and reported with it if the entry is ever found corrupt. All
    /// zeros when the index and epoch identify the entry on their own.
    pub tag: Tag,
    /// The caller's bytes.
    pub payload: &'a [u8],
}

impl<'a> Record<'a> {
    /// A record with an all-zero tag.
    #[must_use]
    pub fn new(epoch: u64, payload: &'a [u8]) -> Self {
        Self {
            epoch,
            tag: [0; TAG_SIZE],
            payload,
        }
    }

    /// The same record carrying `tag`.
    #[must_use]
    pub fn with_tag(self, tag: Tag) -> Self {
        Self { tag, ..self }
    }
}

/// What opening the journal found and repaired.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Recovery {
    /// The directory held no segment and the journal was created.
    pub created: bool,
    /// Damaged entries whose identifier is intact, with the identity it
    /// records. They stay in the log; reading one returns
    /// [`JournalError::Corrupt`]. A replicated caller fixes them from a peer.
    pub corrupt: Vec<EntryId>,
    /// The last entry of the log, when its identifier was intact but the
    /// entry was not. A crash between the slot write and the sync produces
    /// exactly this, and so can corruption; no local algorithm tells them
    /// apart. [`JournalConfig::ambiguous_tail`] says whether it was
    /// truncated like a torn write or kept, marked corrupt, for the caller
    /// to resolve.
    pub ambiguous_tail: Option<EntryId>,
    /// Slots of intact entries that were missing or damaged and rewritten.
    pub slots_rewritten: usize,
    /// Something past the end of the log was discarded and zeroed.
    pub torn_tail: bool,
    /// Segment header copies that were damaged and rewritten from their twin.
    pub headers_repaired: usize,
    /// One copy of the caller's metadata was damaged, missing, or older than
    /// the other, and was rewritten from the newest valid one — so a later
    /// fault in either copy can never roll the metadata back.
    pub meta_repaired: bool,
}

/// A write-ahead journal over moonpool's [`BlockFile`](moonpool_core::BlockFile).
///
/// See the [crate docs](crate) for the layout and the recovery rules.
pub struct Journal<P: StorageProvider> {
    provider: P,
    dir: String,
    config: JournalConfig,
    meta: DualFile,
    meta_value: Option<Vec<u8>>,
    /// Sorted by first index; never empty. The last one takes appends.
    segments: Vec<Segment<P::File>>,
    poisoned: bool,
}

impl<P: StorageProvider> std::fmt::Debug for Journal<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("dir", &self.dir)
            .field("start", &self.start_index())
            .field("next", &self.next_index())
            .field("segments", &self.segments.len())
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

fn parent_of(dir: &str) -> &str {
    match dir.trim_end_matches('/').rfind('/') {
        Some(0) => "/",
        Some(at) => &dir[..at],
        None => ".",
    }
}

impl<P: StorageProvider> Journal<P> {
    /// Open the journal under `dir`, creating it if the directory holds no
    /// segment, and run the recovery scan.
    ///
    /// Segments are found by listing the directory: each is named after its
    /// first index, so the names alone give their order. A segment file left
    /// half-created by a crash (`seg-….wal.tmp`) is removed.
    ///
    /// # Errors
    ///
    /// [`JournalError::InvalidConfig`] for an unusable configuration,
    /// [`JournalError::DoubleFault`] where an entry and its identifier are
    /// both damaged, the segment metadata faults, and any I/O error.
    #[instrument(skip(provider, config))]
    pub async fn open(
        provider: P,
        dir: &str,
        config: JournalConfig,
    ) -> Result<(Self, Recovery), JournalError> {
        config.geometry.validate()?;
        provider.create_dir_all(dir).await?;
        provider.sync_dir(parent_of(dir)).await?;

        let (meta, meta_value, meta_repaired) = DualFile::load(&provider, dir, "meta").await?;
        let mut firsts = Vec::new();
        let mut leftovers = false;
        for name in provider.list_dir(dir).await? {
            if let Some(first) = parse_segment_name(&name) {
                firsts.push(first);
            } else if is_segment_leftover(&name) {
                provider.delete(&format!("{dir}/{name}")).await?;
                leftovers = true;
            }
        }
        if leftovers {
            provider.sync_dir(dir).await?;
        }
        firsts.sort_unstable();

        let mut recovery = Recovery {
            meta_repaired,
            ..Recovery::default()
        };
        let mut segments = Vec::with_capacity(firsts.len().max(1));
        if firsts.is_empty() {
            let first = config.first_index;
            segments.push(
                Segment::create(&provider, dir, first, config.geometry, config.direct_io).await?,
            );
            recovery.created = true;
        }
        for (k, first) in firsts.iter().enumerate() {
            let next_first = firsts.get(k + 1).copied();
            let (segment, found) = Segment::recover(
                &provider,
                dir,
                *first,
                config.geometry,
                config.direct_io,
                next_first,
            )
            .await?;
            recovery.corrupt.extend(found.corrupt);
            recovery.slots_rewritten += found.slots_rewritten;
            recovery.torn_tail |= found.torn;
            recovery.headers_repaired += usize::from(found.header_repaired);
            segments.push(segment);
            if found.ended && next_first.is_some() {
                // The log ends inside a sealed segment: everything after it
                // is discarded, newest first so the survivors stay a prefix.
                for later in firsts[k + 1..].iter().rev() {
                    provider.delete(&segment_path(dir, *later)).await?;
                }
                provider.sync_dir(dir).await?;
                recovery.torn_tail = true;
                break;
            }
        }

        let mut journal = Self {
            provider,
            dir: dir.to_string(),
            config,
            meta,
            meta_value,
            segments,
            poisoned: false,
        };
        // The last entry is the one a crash can leave with its identifier
        // but without its bytes: ambiguous. A single node truncates it; a
        // replicated caller may keep it and resolve it with its peers.
        if let Some(rec) = journal.last_record()
            && rec.corrupt
        {
            let id = rec.slot.id();
            recovery.corrupt.retain(|corrupt| corrupt.index != id.index);
            recovery.ambiguous_tail = Some(id);
            tracing::warn!(index = id.index, epoch = id.epoch, policy = ?journal.config.ambiguous_tail, "journal last entry ambiguous");
            if journal.config.ambiguous_tail == AmbiguousTail::Truncate {
                recovery.torn_tail = true;
                journal.truncate_suffix(id.index).await?;
            }
        }
        for id in &recovery.corrupt {
            tracing::warn!(index = id.index, epoch = id.epoch, "journal entry corrupt");
        }
        Ok((journal, recovery))
    }

    /// The segment taking appends. The list is never empty: creation starts
    /// it with one segment and truncation always keeps the first.
    fn tail(&self) -> &Segment<P::File> {
        self.segments
            .last()
            .expect("the segment list is never empty")
    }

    fn tail_mut(&mut self) -> &mut Segment<P::File> {
        self.segments
            .last_mut()
            .expect("the segment list is never empty")
    }

    /// The log's last record, wherever it lives (the tail segment may be
    /// freshly rolled over and still empty).
    fn last_record(&self) -> Option<crate::scan::Rec> {
        self.segments.iter().rev().find_map(Segment::last)
    }

    /// First live index: the first index of the oldest segment.
    #[must_use]
    pub fn start_index(&self) -> u64 {
        self.segments.first().map_or(0, |segment| segment.first)
    }

    /// The index the next appended entry gets.
    #[must_use]
    pub fn next_index(&self) -> u64 {
        self.tail().next_index()
    }

    /// The last index in the log, if the log is not empty.
    #[must_use]
    pub fn last_index(&self) -> Option<u64> {
        let next = self.next_index();
        (next > self.start_index()).then(|| next - 1)
    }

    /// The largest payload one entry may carry: what one segment's data
    /// region holds.
    #[must_use]
    pub fn max_payload(&self) -> u64 {
        // Entries are padded to 8 bytes.
        (self.config.geometry.data_capacity() & !7) - ENTRY_HEADER_SIZE as u64
    }

    fn segment_of(&self, index: u64) -> Result<&Segment<P::File>, JournalError> {
        let (start, next) = (self.start_index(), self.next_index());
        if index < start || index >= next {
            return Err(JournalError::OutOfRange { index, start, next });
        }
        // Binary search: the largest first index at or below `index`.
        let at = self
            .segments
            .partition_point(|segment| segment.first <= index)
            - 1;
        Ok(&self.segments[at])
    }

    /// The epoch recorded for `index`, without I/O: the startup scan already
    /// verified every slot.
    #[must_use]
    pub fn epoch(&self, index: u64) -> Option<u64> {
        self.entry_id(index).map(|id| id.epoch)
    }

    /// The identity — index, epoch, and tag — recorded for `index`, without
    /// I/O, whether or not the entry itself is intact.
    #[must_use]
    pub fn entry_id(&self, index: u64) -> Option<EntryId> {
        self.segment_of(index).ok()?.id(index)
    }

    /// Read and verify the entry at `index`.
    ///
    /// # Errors
    ///
    /// [`JournalError::OutOfRange`] outside `[start, next)`,
    /// [`JournalError::Corrupt`] if the entry fails any check (its CRC, its
    /// index, or agreement with its slot) or the medium cannot read it, and
    /// any other I/O error.
    pub async fn read(&self, index: u64) -> Result<Entry, JournalError> {
        let entry = self.segment_of(index)?.read(index).await;
        if let Err(JournalError::Corrupt(id)) = &entry {
            tracing::warn!(
                index = id.index,
                epoch = id.epoch,
                "journal entry corrupt on read"
            );
        }
        entry
    }

    /// Read and verify every entry in `range`, in order — the replay a
    /// caller runs after [`open`](Self::open) to rebuild its state.
    ///
    /// Entries are packed back to back, so each segment's share of the
    /// range is read in large sequential transfers rather than one read per
    /// entry. Each entry comes back intact, with exactly the checks
    /// [`read`](Self::read) applies, or as `Err` with the identity its slot
    /// records: a corrupt entry does not stop the replay, it is reported in
    /// place.
    ///
    /// # Errors
    ///
    /// [`JournalError::OutOfRange`] unless `range` lies within
    /// `[start, next]` and is not reversed, and any I/O error other than the
    /// medium failing to read an entry (which reports that entry corrupt).
    pub async fn read_range(
        &self,
        range: Range<u64>,
    ) -> Result<Vec<Result<Entry, EntryId>>, JournalError> {
        let (start, next) = (self.start_index(), self.next_index());
        for bound in [range.start, range.end] {
            if bound < start || bound > next || range.start > range.end {
                return Err(JournalError::OutOfRange {
                    index: bound,
                    start,
                    next,
                });
            }
        }
        let mut out = Vec::with_capacity(usize::try_from(range.end - range.start).unwrap_or(0));
        let mut index = range.start;
        while index < range.end {
            let segment = self.segment_of(index)?;
            let stop = range.end.min(segment.next_index());
            segment.read_range(index..stop, &mut out).await?;
            index = stop;
        }
        for id in out.iter().filter_map(|entry| entry.as_ref().err()) {
            tracing::warn!(
                index = id.index,
                epoch = id.epoch,
                "journal entry corrupt on read"
            );
        }
        Ok(out)
    }

    fn check_writable(&self) -> Result<(), JournalError> {
        if self.poisoned {
            Err(JournalError::Poisoned)
        } else {
            Ok(())
        }
    }

    /// Pass `outcome` through, poisoning the journal when it is an error: a
    /// failed write leaves the on-disk state no longer known to match memory.
    fn poison_on_err<T>(&mut self, outcome: Result<T, JournalError>) -> Result<T, JournalError> {
        if outcome.is_err() {
            self.poisoned = true;
        }
        outcome
    }

    /// Append `records` at [`next_index`](Self::next_index) as one batch and
    /// return the indexes they got. On `Ok` every record is durable.
    ///
    /// The entries go in one write, their slots in another, then one
    /// `fdatasync`. A batch that does not fit the current segment's slot
    /// table or data region fills it, and the rest continues in a new
    /// segment with its own sync.
    ///
    /// # Errors
    ///
    /// [`JournalError::EntryTooLarge`] (nothing is written),
    /// [`JournalError::Poisoned`] after an earlier failure, and any I/O error
    /// — which poisons the journal, since part of the append may be on disk.
    #[instrument(skip_all, fields(dir = %self.dir, count = records.len()))]
    pub async fn append(&mut self, records: &[Record<'_>]) -> Result<Range<u64>, JournalError> {
        self.check_writable()?;
        let max = self.max_payload();
        let mut sizes = Vec::with_capacity(records.len());
        for record in records {
            let len = u32::try_from(record.payload.len())
                .ok()
                .filter(|len| u64::from(*len) <= max)
                .ok_or(JournalError::EntryTooLarge {
                    len: record.payload.len(),
                    max,
                })?;
            sizes.push(entry_size(len));
        }
        let first = self.next_index();
        let mut done = 0;
        while done < records.len() {
            let fit = self.tail().fitting(&sizes[done..]);
            let outcome = if fit == 0 {
                self.rollover().await
            } else {
                self.tail_mut().append(&records[done..done + fit]).await
            };
            self.poison_on_err(outcome)?;
            done += fit;
        }
        Ok(first..self.next_index())
    }

    /// Start a new segment at the next index. Every batch in the current one
    /// is already synced.
    async fn rollover(&mut self) -> Result<(), JournalError> {
        let first = self.next_index();
        let segment = Segment::create(
            &self.provider,
            &self.dir,
            first,
            self.config.geometry,
            self.config.direct_io,
        )
        .await?;
        self.segments.push(segment);
        tracing::debug!(first, "journal segment rolled over");
        Ok(())
    }

    /// Discard every entry at `from` and after (Raft suffix truncation).
    ///
    /// Segments wholly past the cut are deleted, newest first. In the segment
    /// the cut falls in, the discarded slots are zeroed and synced, then the
    /// discarded entries are zeroed and synced — the clean-up that must
    /// precede the next append, or a crash could leave an old slot beside a
    /// new entry and make a harmless crash look like corruption. A crash
    /// part-way leaves a log that ends somewhere between `from` and the old
    /// end: never a gap, never a corrupt entry.
    ///
    /// # Errors
    ///
    /// [`JournalError::OutOfRange`] unless `start <= from <= next`,
    /// [`JournalError::Poisoned`], and any I/O error (which poisons).
    #[instrument(skip(self), fields(dir = %self.dir))]
    pub async fn truncate_suffix(&mut self, from: u64) -> Result<(), JournalError> {
        self.check_writable()?;
        let (start, next) = (self.start_index(), self.next_index());
        if from < start || from > next {
            return Err(JournalError::OutOfRange {
                index: from,
                start,
                next,
            });
        }
        if from == next {
            return Ok(());
        }
        // Keep the segments that start before the cut, and always the first.
        let keep = self
            .segments
            .partition_point(|segment| segment.first < from)
            .max(1);
        let outcome = async {
            if keep < self.segments.len() {
                // Newest first, so the survivors always stay a prefix.
                for dropped in self.segments.drain(keep..).rev() {
                    let path = segment_path(&self.dir, dropped.first);
                    drop(dropped);
                    self.provider.delete(&path).await?;
                }
                self.provider.sync_dir(&self.dir).await?;
            }
            self.tail_mut().truncate_from(from).await
        }
        .await;
        self.poison_on_err(outcome)
    }

    /// Forget whole segments that lie entirely before `before` (compaction).
    /// The segment holding `before` is kept, so the new
    /// [`start_index`](Self::start_index) is its first index — at or below
    /// `before`. Deleted oldest first, so the survivors stay a suffix.
    ///
    /// # Errors
    ///
    /// [`JournalError::OutOfRange`] unless `start <= before <= next`,
    /// [`JournalError::Poisoned`], and any I/O error (which poisons).
    #[instrument(skip(self), fields(dir = %self.dir))]
    pub async fn truncate_prefix(&mut self, before: u64) -> Result<(), JournalError> {
        self.check_writable()?;
        let (start, next) = (self.start_index(), self.next_index());
        if before < start || before > next {
            return Err(JournalError::OutOfRange {
                index: before,
                start,
                next,
            });
        }
        let drop_count = self
            .segments
            .partition_point(|segment| segment.first <= before)
            .saturating_sub(1);
        if drop_count == 0 {
            return Ok(());
        }
        let outcome = async {
            for dropped in self.segments.drain(..drop_count) {
                let path = segment_path(&self.dir, dropped.first);
                drop(dropped);
                self.provider.delete(&path).await?;
            }
            self.provider.sync_dir(&self.dir).await?;
            Ok(())
        }
        .await;
        self.poison_on_err(outcome)
    }

    /// The caller's metadata (for example Raft's term and vote), as last
    /// saved.
    #[must_use]
    pub fn meta(&self) -> Option<&[u8]> {
        self.meta_value.as_deref()
    }

    /// Durably replace the caller's metadata. It lives in its own file, in
    /// two copies (`meta.0`, `meta.1`), each with a generation counter and a
    /// CRC, each updated through a temporary file, a sync, and a rename.
    ///
    /// # Errors
    ///
    /// Any I/O error; at least one copy is still intact on disk.
    #[instrument(skip_all, fields(dir = %self.dir, len = bytes.len()))]
    pub async fn save_meta(&mut self, bytes: &[u8]) -> Result<(), JournalError> {
        self.meta.store(&self.provider, bytes).await?;
        self.meta_value = Some(bytes.to_vec());
        Ok(())
    }
}
