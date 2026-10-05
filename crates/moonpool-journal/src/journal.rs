//! The journal: the ordered segments of a directory, and the caller's
//! metadata beside them.

use std::ops::Range;

use moonpool_core::{DirectIo, StorageProvider};
use tracing::instrument;

use crate::dual::DualFile;
use crate::layout::{ENTRY_HEADER_SIZE, Geometry, TAG_SIZE, Tag, entry_size};
use crate::segment::{Entry, Segment, is_segment_leftover, parse_segment_name, segment_path};
use crate::{EntryId, JournalAtlas, JournalError};

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
    /// What opening does with the damaged entries of the last batch.
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

/// What opening does with the damaged entries of the log's last append
/// batch — entries whose identifier is intact but whose bytes are not.
///
/// A batch's entries and slots are written by two unordered writes and made
/// durable by one `fdatasync`. A crash before that sync returned can land
/// any subset of their sectors, so any entry of the last batch can come back
/// with its identifier and without its bytes — and so can corruption of a
/// batch that was synced and acknowledged. No local algorithm tells the two
/// apart (the CLSTORE paper's Appendix A, which the paper states for the
/// last entry; with batched appends it holds for the whole last batch,
/// since nothing durable after it proves its sync returned). Either way the
/// entries are reported in [`Recovery::ambiguous_batch`], never in
/// [`Recovery::corrupt`]; this decides whether they are also removed.
///
/// Batches are found through a batch-start flag every entry and slot carry,
/// so a caller never has to encode batch numbers of its own. A batch that
/// rolls over into a new segment is two batches, each with its own sync.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AmbiguousTail {
    /// Treat them as a torn write: truncate from the first damaged entry of
    /// the last batch, with everything after it — right for a single node,
    /// whose only other choice would be to refuse to start. A replicated
    /// caller that instead needs to find out whether they were committed
    /// must act on the report before the next crash: once truncated, the
    /// identities are gone from disk.
    #[default]
    Truncate,
    /// Keep them in the log, marked corrupt like a damaged entry mid-log: a
    /// read returns [`JournalError::Corrupt`], and their identities survive
    /// every later reopen. The caller decides — for example by asking its
    /// peers whether each was committed — and discards them with
    /// [`Journal::truncate_suffix`] if they were not. Appending after them
    /// starts a new batch and leaves them ordinary corrupt entries,
    /// reported in [`Recovery::corrupt`] from then on.
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
    /// Damaged entries whose identifier is intact, before the log's last
    /// batch, with the identity their slot records: a later sync covers
    /// them, so they were durable and damage explains them, never a crash.
    /// They stay in the log; reading one returns [`JournalError::Corrupt`].
    /// A replicated caller fixes them from a peer.
    pub corrupt: Vec<EntryId>,
    /// Damaged entries of the log's last append batch whose identifier is
    /// intact, in index order. A crash before the batch's sync returned
    /// produces exactly this, and so can corruption; no local algorithm
    /// tells them apart. [`JournalConfig::ambiguous_tail`] says whether
    /// they were truncated like a torn write or kept, marked corrupt, for
    /// the caller to resolve.
    pub ambiguous_batch: Vec<EntryId>,
    /// Slots of intact entries that were missing or damaged and rewritten.
    pub slots_rewritten: usize,
    /// Something past the end of the log was discarded: a torn identifier
    /// in the last batch ended the log, or slots or entry blocks past the
    /// end were reset.
    pub torn_tail: bool,
    /// Segment header copies that were damaged and rewritten from their twin.
    pub headers_repaired: usize,
    /// One copy of the caller's metadata was damaged, missing, or older than
    /// the other, and was rewritten from the newest valid one — so a later
    /// fault in either copy can never roll the metadata back.
    pub meta_repaired: bool,
    /// The newest intact checkpoint batch ([`Journal::append_checkpoint`]),
    /// if any: where a replay starts. A caller replays
    /// `checkpoint.start..next_index()` and prepends nothing. A checkpoint
    /// with a damaged entry is passed over for the one before it.
    pub checkpoint: Option<Range<u64>>,
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
    /// The log's start, made durable by every prefix truncation before it
    /// deletes anything (`start.0`, `start.1`).
    start: DualFile,
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

/// Every directory whose entries name a component of `dir`, from the root
/// down: `.`, `a`, `a/b` for `a/b/c`; `/`, `/a` for `/a/b`.
///
/// Syncing each of them makes the whole chain of names that reaches `dir`
/// durable. A name created by `create_dir_all` is lost in a crash unless its
/// parent is synced, and a synced child does not survive the loss of its
/// parent's own name, so syncing only `dir`'s immediate parent is not enough
/// for a nested `dir`.
fn ancestors_of(dir: &str) -> Vec<String> {
    let absolute = dir.starts_with('/');
    let components: Vec<&str> = dir
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let mut parents = Vec::with_capacity(components.len());
    let mut current = if absolute {
        "/".to_string()
    } else {
        ".".to_string()
    };
    for component in components {
        parents.push(current.clone());
        current = match current.as_str() {
            "." => component.to_string(),
            "/" => format!("/{component}"),
            _ => format!("{current}/{component}"),
        };
    }
    parents
}

/// The first indexes of the segments under `dir`, in order, after removing
/// what a crash left behind: half-created segment files, and segments wholly
/// below the `recorded_start` (unlinks a crash undid).
///
/// # Errors
///
/// [`JournalError::SegmentGap`] when the segment holding the recorded start
/// is missing, and any I/O error.
async fn segment_firsts<P: StorageProvider>(
    provider: &P,
    dir: &str,
    recorded_start: Option<u64>,
) -> Result<Vec<u64>, JournalError> {
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
    if let Some(start) = recorded_start {
        // A prefix truncation records the new start before its unlinks,
        // which a crash may undo one by one: a segment wholly below the
        // start is one of those, back from the dead, and goes again.
        let below = firsts
            .windows(2)
            .take_while(|pair| pair[1] <= start)
            .count();
        for first in firsts.drain(..below) {
            tracing::warn!(first, start, "journal segment below the start removed");
            provider.delete(&segment_path(dir, first)).await?;
        }
        if below > 0 {
            provider.sync_dir(dir).await?;
        }
        if let Some(first) = firsts.first().copied()
            && first != start
        {
            return Err(JournalError::SegmentGap {
                end: start,
                next: first,
            });
        }
    }

    Ok(firsts)
}

/// The start a prefix truncation recorded: eight little-endian bytes.
fn decode_start(bytes: &[u8]) -> Result<u64, JournalError> {
    bytes
        .try_into()
        .map(u64::from_le_bytes)
        .map_err(|_| JournalError::MetadataCorrupt { name: "start" })
}

impl<P: StorageProvider> Journal<P> {
    /// Open the journal under `dir`, creating it if the directory holds no
    /// segment, and run the recovery scan.
    ///
    /// Segments are found by listing the directory: each is named after its
    /// first index, so the names alone give their order. Everything the
    /// recovered log holds is synced before this returns: a process that
    /// restarted without a power loss reads its predecessor's unsynced
    /// writes back, and an entry reported here must not vanish at the next
    /// crash. A segment file left
    /// half-created by a crash (`seg-….wal.tmp`) is removed.
    ///
    /// `dir` may be nested: every directory on the path to it is created and
    /// its name made durable, so a crash right after the first open cannot
    /// drop an ancestor and with it the whole journal. `dir` itself is synced
    /// too, before anything in it is trusted: a name an earlier, failed
    /// attempt left visible but not durable is made durable first.
    ///
    /// # Errors
    ///
    /// [`JournalError::InvalidConfig`] for an unusable configuration,
    /// [`JournalError::DoubleFault`] where an entry and its identifier are
    /// both damaged before the last batch, [`JournalError::SegmentGap`]
    /// where a segment is missing between the start and the tail, the
    /// segment metadata faults, and any I/O error.
    #[instrument(skip(provider, config))]
    pub async fn open(
        provider: P,
        dir: &str,
        config: JournalConfig,
    ) -> Result<(Self, Recovery), JournalError> {
        config.geometry.validate()?;
        provider.create_dir_all(dir).await?;
        // Every ancestor, not only the ones this call created: an earlier open
        // may have created them and failed before its syncs completed, leaving
        // names that are visible but not durable.
        for parent in ancestors_of(dir) {
            provider.sync_dir(&parent).await?;
        }
        // And the directory itself: a segment or metadata copy renamed into
        // place by an earlier attempt whose directory sync failed is visible
        // but not durable. Recovering it without this sync would acknowledge
        // appends into a file whose name the next crash can undo.
        provider.sync_dir(dir).await?;

        let (meta, meta_value, meta_repaired) = DualFile::load(&provider, dir, "meta").await?;
        let (start, recorded_start, _) = DualFile::load(&provider, dir, "start").await?;
        let recorded_start = recorded_start
            .map(|bytes| decode_start(&bytes))
            .transpose()?;
        let firsts = segment_firsts(&provider, dir, recorded_start).await?;

        let mut recovery = Recovery {
            meta_repaired,
            ..Recovery::default()
        };
        let mut segments = Vec::with_capacity(firsts.len().max(1));
        if firsts.is_empty() {
            let first = recorded_start.unwrap_or(config.first_index);
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
        }

        let mut journal = Self {
            provider,
            dir: dir.to_string(),
            config,
            meta,
            meta_value,
            start,
            segments,
            poisoned: false,
        };
        // The last batch is the one a crash can leave with identifiers but
        // without bytes: its damaged entries are ambiguous. A single node
        // truncates them; a replicated caller may keep them and resolve them
        // with its peers.
        let ambiguous: Vec<EntryId> = journal
            .last_batch()
            .iter()
            .filter(|rec| rec.corrupt)
            .map(|rec| rec.slot.id())
            .collect();
        if let Some(first) = ambiguous.first() {
            recovery
                .corrupt
                .retain(|corrupt| corrupt.index < first.index);
            for id in &ambiguous {
                tracing::warn!(
                    index = id.index,
                    epoch = id.epoch,
                    policy = ?journal.config.ambiguous_tail,
                    "journal entry of the last batch ambiguous"
                );
            }
            if journal.config.ambiguous_tail == AmbiguousTail::Truncate {
                recovery.torn_tail = true;
                journal.truncate_suffix(first.index).await?;
            }
            recovery.ambiguous_batch = ambiguous;
        }
        recovery.checkpoint = journal.checkpoint();
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

    /// The records of the log's last append batch, wherever it lives (the
    /// tail segment may be freshly rolled over and still empty). Every
    /// segment's first batch starts it, so the last batch never spans two.
    fn last_batch(&self) -> &[crate::scan::Rec] {
        self.segments
            .iter()
            .rev()
            .find(|segment| segment.last().is_some())
            .map_or(&[], |segment| segment.last_batch())
    }

    /// Where every region recovery reads lives right now: the metadata
    /// copies, each segment's headers, and each live entry and its slot.
    /// See [`JournalAtlas`].
    #[must_use]
    pub fn atlas(&self) -> JournalAtlas {
        let mut atlas = JournalAtlas::default();
        if let Some(value) = &self.meta_value {
            for copy in 0..2u8 {
                atlas.push_meta(
                    &self.meta.path(u64::from(copy)),
                    copy,
                    u64::try_from(value.len()).unwrap_or(u64::MAX),
                );
            }
        }
        if self.start.is_stored() {
            for copy in 0..2u8 {
                atlas.push_start(&self.start.path(u64::from(copy)), copy);
            }
        }
        for segment in &self.segments {
            segment.chart(&segment_path(&self.dir, segment.first), &mut atlas);
        }
        let batch = self.last_batch();
        let last_batch = batch.first().map_or(0..0, |rec| {
            rec.slot.index..rec.slot.index + u64::try_from(batch.len()).unwrap_or(u64::MAX)
        });
        atlas.set_live(self.start_index()..self.next_index(), last_batch);
        atlas
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
        let sizes = self.sizes(records)?;
        let first = self.next_index();
        let mut done = 0;
        while done < records.len() {
            let fit = self.tail().fitting(&sizes[done..]);
            let outcome = if fit == 0 {
                self.rollover().await
            } else {
                self.tail_mut()
                    .append(&records[done..done + fit], false)
                    .await
            };
            self.poison_on_err(outcome)?;
            done += fit;
        }
        Ok(first..self.next_index())
    }

    /// The on-disk size of each record.
    ///
    /// # Errors
    ///
    /// [`JournalError::EntryTooLarge`] for a payload one segment cannot hold.
    fn sizes(&self, records: &[Record<'_>]) -> Result<Vec<u64>, JournalError> {
        let max = self.max_payload();
        records
            .iter()
            .map(|record| {
                u32::try_from(record.payload.len())
                    .ok()
                    .filter(|len| u64::from(*len) <= max)
                    .map(entry_size)
                    .ok_or(JournalError::EntryTooLarge {
                        len: record.payload.len(),
                        max,
                    })
            })
            .collect()
    }

    /// Append `records` as one batch flagged as a checkpoint: once durable it
    /// supersedes every entry before it. Returns the indexes they got.
    ///
    /// A checkpoint is the caller's state re-emitted, so that a replay can
    /// start from it: [`checkpoint`](Self::checkpoint) and
    /// [`Recovery::checkpoint`] name the newest intact one, and the caller
    /// replays `checkpoint.start..next_index()`. Dropping the history before
    /// it stays the caller's [`truncate_prefix`](Self::truncate_prefix)
    /// (`checkpoint.start`), so a crash between the two is harmless: the old
    /// history is still there. When to checkpoint is the caller's call too.
    ///
    /// The batch is never split: if the current segment cannot hold all of
    /// it, the journal rolls over first, so a checkpoint is always one batch
    /// with one sync. Its first entry and slot record its entry count, so a
    /// checkpoint cut short — by a crash before its sync, or by a suffix
    /// truncation — is never taken for a complete one. A crash leaves it the
    /// log's last batch, handled like any other (its damaged entries
    /// truncated or kept per [`JournalConfig::ambiguous_tail`], a torn end
    /// ending the log); whatever survives of it stays in the log, never
    /// named, and a replay from the named checkpoint skips it as the caller
    /// sees fit (the caller knows its image records).
    ///
    /// # Errors
    ///
    /// [`JournalError::CheckpointTooLarge`] when an empty segment could not
    /// hold it, and with no records at all; [`JournalError::EntryTooLarge`]
    /// (nothing is written in either case), [`JournalError::Poisoned`], and
    /// any I/O error, which poisons.
    #[instrument(skip_all, fields(dir = %self.dir, count = records.len()))]
    pub async fn append_checkpoint(
        &mut self,
        records: &[Record<'_>],
    ) -> Result<Range<u64>, JournalError> {
        self.check_writable()?;
        let sizes = self.sizes(records)?;
        let bytes: u64 = sizes.iter().sum();
        let geometry = self.config.geometry;
        if records.is_empty()
            || u64::try_from(records.len()).map_or(true, |n| n > u64::from(geometry.slot_count))
            || bytes > geometry.data_capacity()
        {
            return Err(JournalError::CheckpointTooLarge {
                entries: records.len(),
                bytes,
            });
        }
        if self.tail().fitting(&sizes) < records.len() {
            let outcome = self.rollover().await;
            self.poison_on_err(outcome)?;
        }
        let first = self.next_index();
        let outcome = self.tail_mut().append(records, true).await;
        self.poison_on_err(outcome)?;
        Ok(first..self.next_index())
    }

    /// The newest intact checkpoint batch: every entry
    /// [`append_checkpoint`](Self::append_checkpoint) wrote for it is in the
    /// log and none is damaged. A damaged one, or one cut short, is passed
    /// over for the one before it. `None` when there is none.
    #[must_use]
    pub fn checkpoint(&self) -> Option<Range<u64>> {
        for segment in self.segments.iter().rev() {
            let recs = segment.records();
            for (at, rec) in recs.iter().enumerate().rev() {
                let Some(count) = rec.slot.checkpoint else {
                    continue;
                };
                let Some(batch) = usize::try_from(count)
                    .ok()
                    .and_then(|count| recs.get(at..at.checked_add(count)?))
                else {
                    continue;
                };
                let whole = batch
                    .iter()
                    .skip(1)
                    .all(|rec| !rec.slot.batch_start && !rec.corrupt);
                if !rec.corrupt && whole {
                    let start = rec.slot.index;
                    return Some(start..start + u64::from(count));
                }
            }
        }
        None
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
    /// the cut falls in, the discarded slots are reset to their reserved
    /// records and synced, then the discarded entries' blocks are zeroed and
    /// synced — the clean-up that must precede the next append, or a crash
    /// could leave an old slot beside a new entry and make a harmless crash
    /// look like corruption. The block holding the cut keeps its kept
    /// entries and is never rewritten. A crash part-way leaves a log that
    /// ends somewhere between `from` and the old end: never a gap, never a
    /// corrupt entry.
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
                // Newest first, each unlink durable before the next, so a
                // crash can undo at most the last one: the survivors always
                // stay a prefix, never one with a hole.
                for dropped in self.segments.drain(keep..).rev() {
                    let path = segment_path(&self.dir, dropped.first);
                    drop(dropped);
                    self.provider.delete(&path).await?;
                    self.provider.sync_dir(&self.dir).await?;
                }
            }
            self.tail_mut().truncate_from(from).await
        }
        .await;
        self.poison_on_err(outcome)
    }

    /// Forget whole segments that lie entirely before `before` (compaction).
    /// The segment holding `before` is kept, so the new
    /// [`start_index`](Self::start_index) is its first index — at or below
    /// `before`.
    ///
    /// The new start is made durable first, in its own two-copy record
    /// (`start.0`, `start.1`), and only then are the segments deleted, oldest
    /// first. A crash can undo any of those unlinks, each on its own; the
    /// next open finds the durable start and deletes again whatever lies
    /// wholly below it, so a resurrected segment is never read as part of
    /// the log.
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
        let new_start = self.segments[drop_count].first;
        let outcome = async {
            self.start
                .store(&self.provider, &new_start.to_le_bytes())
                .await?;
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

    /// The caller's metadata under `dir` as the newest valid copy holds it,
    /// without opening the journal: no recovery scan, no repair of a damaged
    /// or older copy, no truncation, nothing created. What
    /// [`meta`](Self::meta) would return after [`open`](Self::open), for a
    /// caller that only needs to know — say, whether a store was ever
    /// formatted — and must not change what the next open finds.
    ///
    /// Returns `None` when neither copy exists, including when `dir` does
    /// not.
    ///
    /// # Errors
    ///
    /// [`JournalError::MetadataCorrupt`] when a copy exists but none is
    /// valid (or both are valid at one generation and disagree), and
    /// [`JournalError::Io`] if the namespace cannot be queried.
    pub async fn peek_meta(provider: &P, dir: &str) -> Result<Option<Vec<u8>>, JournalError> {
        DualFile::peek(provider, dir, "meta").await
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
