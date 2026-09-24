//! The journal: an ordered list of segments, a manifest naming them, and the
//! caller's metadata beside them.

use std::ops::Range;

use moonpool_core::{DirectIo, StorageProvider};
use tracing::instrument;

use crate::JournalError;
use crate::dual::DualFile;
use crate::layout::{BLOCK_U64, ENTRY_HEADER_SIZE, Geometry, entry_size};
use crate::segment::{Bound, Entry, Placement, Segment, segment_path};

/// How to lay out and drive a journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalConfig {
    /// The shape of every segment. Must match what an existing journal was
    /// created with: a segment whose header disagrees is refused.
    pub geometry: Geometry,
    /// The most bytes one `write → write → fdatasync` cycle carries. Larger
    /// appends are split into several cycles.
    ///
    /// This also bounds how far past the last intact entry an unacknowledged
    /// write can have reached when the process crashed, which is the range
    /// recovery scrubs before the journal accepts new appends. One entry must
    /// fit in it.
    pub max_batch_bytes: u64,
    /// Direct-I/O policy for segment files.
    pub direct_io: DirectIo,
    /// The index of the first entry of a freshly created journal. Ignored
    /// when the journal already exists.
    pub first_index: u64,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            geometry: Geometry::default(),
            max_batch_bytes: 4 << 20,
            direct_io: DirectIo::Optional,
            first_index: 1,
        }
    }
}

/// An entry to append: the epoch (term) it belongs to and its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    /// The epoch this entry was written in.
    pub epoch: u64,
    /// The caller's bytes.
    pub payload: &'a [u8],
}

/// What opening the journal found and repaired.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Recovery {
    /// The journal did not exist and was created.
    pub created: bool,
    /// Damaged entries mid-log, as `(index, epoch)`. They stay in the log;
    /// reading one returns [`JournalError::Corrupt`]. A replicated caller
    /// re-fetches them from a peer.
    pub corrupt: Vec<(u64, u64)>,
    /// Entries at the tail whose slot was intact but whose entry was not, as
    /// `(index, epoch)`. A single node cannot tell whether they were
    /// acknowledged, so they were truncated like a torn write; a replicated
    /// caller keeps them if they were committed, and discards them if not.
    pub ambiguous_tail: Vec<(u64, u64)>,
    /// Slots of intact entries that were lost or damaged and rewritten.
    pub slots_rewritten: usize,
    /// A torn tail was found and wiped.
    pub torn_tail: bool,
    /// Segment header copies that were damaged and rewritten from their twin.
    pub headers_repaired: usize,
}

/// A write-ahead journal over moonpool's [`BlockFile`](moonpool_core::BlockFile).
///
/// See the [crate docs](crate) for the layout and the recovery rules.
pub struct Journal<P: StorageProvider> {
    provider: P,
    dir: String,
    config: JournalConfig,
    manifest: DualFile,
    meta: DualFile,
    meta_value: Option<Vec<u8>>,
    /// First live index; everything below was compacted away.
    start: u64,
    /// Sorted by first index; never empty. The last one takes appends.
    segments: Vec<Segment<P::File>>,
    poisoned: bool,
}

impl<P: StorageProvider> std::fmt::Debug for Journal<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("dir", &self.dir)
            .field("start", &self.start)
            .field("next", &self.next_index())
            .field("segments", &self.segments.len())
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

/// The manifest: which segments exist, where the live log starts, and a
/// suffix truncation in progress, if any.
///
/// ```text
/// u64 start   u64 fence (u64::MAX: none)   u64 first index of each segment …
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
struct Manifest {
    start: u64,
    fence: Option<u64>,
    firsts: Vec<u64>,
}

impl Manifest {
    const NO_FENCE: u64 = u64::MAX;

    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(16 + 8 * self.firsts.len());
        bytes.extend_from_slice(&self.start.to_le_bytes());
        bytes.extend_from_slice(&self.fence.unwrap_or(Self::NO_FENCE).to_le_bytes());
        for first in &self.firsts {
            bytes.extend_from_slice(&first.to_le_bytes());
        }
        bytes
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 24 || !bytes.len().is_multiple_of(8) {
            return None;
        }
        let mut words = bytes
            .chunks_exact(8)
            .map(|word| u64::from_le_bytes(word.try_into().expect("8 bytes")));
        let start = words.next()?;
        let fence = words.next().filter(|fence| *fence != Self::NO_FENCE);
        let firsts: Vec<u64> = words.collect();
        let sorted = firsts.windows(2).all(|pair| pair[0] < pair[1]);
        let last = *firsts.last()?;
        let fence_ok = fence.is_none_or(|fence| fence >= start.max(last));
        (sorted && firsts[0] <= start && fence_ok).then_some(Self {
            start,
            fence,
            firsts,
        })
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
    /// Open the journal under `dir`, creating it if it does not exist, and
    /// run the recovery scan.
    ///
    /// # Errors
    ///
    /// [`JournalError::InvalidConfig`] for an unusable configuration,
    /// [`JournalError::DoubleFault`] / [`JournalError::MissingEntry`] when
    /// damage cannot be told apart from lost acknowledged data, the
    /// segment-level faults, and any I/O error.
    #[instrument(skip(provider, config))]
    pub async fn open(
        provider: P,
        dir: &str,
        config: JournalConfig,
    ) -> Result<(Self, Recovery), JournalError> {
        config.geometry.validate()?;
        if config.max_batch_bytes < BLOCK_U64 {
            return Err(JournalError::InvalidConfig(
                "max_batch_bytes must hold at least one block".into(),
            ));
        }
        provider.create_dir_all(dir).await?;
        provider.sync_dir(parent_of(dir)).await?;

        let (mut manifest, listed) = DualFile::load(&provider, dir, "manifest").await?;
        let (meta, meta_value) = DualFile::load(&provider, dir, "meta").await?;
        let mut recovery = Recovery::default();
        let (start, segments) = if let Some(bytes) = listed {
            let Manifest {
                start,
                fence,
                firsts,
            } = Manifest::decode(&bytes)
                .ok_or(JournalError::MetadataCorrupt { name: "manifest" })?;
            let mut segments = Vec::with_capacity(firsts.len());
            for (k, first) in firsts.iter().enumerate() {
                let bound = match (firsts.get(k + 1), fence) {
                    (Some(next), _) => Bound::Sealed(*next),
                    (None, Some(fence)) => Bound::Fenced(fence),
                    (None, None) => Bound::Tail,
                };
                let placement = Placement {
                    dir,
                    geometry: config.geometry,
                    direct_io: config.direct_io,
                    start,
                    max_batch: config.max_batch_bytes,
                };
                let (segment, found) =
                    Segment::recover(&provider, &placement, *first, bound).await?;
                recovery.corrupt.extend(found.corrupt);
                recovery.ambiguous_tail.extend(found.ambiguous_tail);
                recovery.slots_rewritten += found.slots_rewritten;
                recovery.torn_tail |= found.torn;
                recovery.headers_repaired += usize::from(found.header_repaired);
                segments.push(segment);
            }
            if fence.is_some() {
                // The interrupted truncation is finished now: clear it.
                recovery.torn_tail = true;
                let cleared = Manifest {
                    start,
                    fence: None,
                    firsts,
                };
                manifest.store(&provider, &cleared.encode()).await?;
            }
            (start, segments)
        } else {
            // No manifest: the journal was never created, or its creation
            // never reached the manifest. Either way nothing in it is live.
            let first = config.first_index;
            let segment =
                Segment::create(&provider, dir, first, config.geometry, config.direct_io).await?;
            let created = Manifest {
                start: first,
                fence: None,
                firsts: vec![first],
            };
            manifest.store(&provider, &created.encode()).await?;
            recovery.created = true;
            (first, vec![segment])
        };

        let next = segments.last().map_or(start, Segment::next_index);
        if next < start {
            return Err(JournalError::MissingEntry { index: next });
        }
        for (index, epoch) in &recovery.corrupt {
            tracing::warn!(index, epoch, "journal entry corrupt");
        }
        let journal = Self {
            provider,
            dir: dir.to_string(),
            config,
            manifest,
            meta,
            meta_value,
            start,
            segments,
            poisoned: false,
        };
        Ok((journal, recovery))
    }

    /// The segment taking appends. The list is never empty: creation starts
    /// it with one segment and truncation always keeps the last.
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

    /// Durably record the segment list (the first `count` segments), the
    /// live start, and a truncation fence.
    async fn publish(&mut self, count: usize, fence: Option<u64>) -> Result<(), JournalError> {
        let manifest = Manifest {
            start: self.start,
            fence,
            firsts: self.segments[..count].iter().map(|s| s.first).collect(),
        };
        self.manifest
            .store(&self.provider, &manifest.encode())
            .await
    }

    /// First live index.
    #[must_use]
    pub fn start_index(&self) -> u64 {
        self.start
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
        (next > self.start).then(|| next - 1)
    }

    /// The largest payload one entry may carry.
    #[must_use]
    pub fn max_payload(&self) -> u64 {
        let per_entry = self
            .config
            .max_batch_bytes
            .min(self.config.geometry.data_capacity());
        // Entries are padded to 8 bytes.
        (per_entry & !7) - ENTRY_HEADER_SIZE as u64
    }

    fn segment_of(&self, index: u64) -> Result<&Segment<P::File>, JournalError> {
        let next = self.next_index();
        if index < self.start || index >= next {
            return Err(JournalError::OutOfRange {
                index,
                start: self.start,
                next,
            });
        }
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
        self.segment_of(index).ok()?.epoch(index)
    }

    /// Read and verify the entry at `index`.
    ///
    /// # Errors
    ///
    /// [`JournalError::OutOfRange`] outside `[start, next)`,
    /// [`JournalError::Corrupt`] if the entry fails any check (its CRC, its
    /// index, or agreement with its slot), and any I/O error.
    pub async fn read(&self, index: u64) -> Result<Entry, JournalError> {
        let entry = self.segment_of(index)?.read(index).await;
        if let Err(JournalError::Corrupt { index, epoch }) = &entry {
            tracing::warn!(index, epoch, "journal entry corrupt on read");
        }
        entry
    }

    fn check_writable(&self) -> Result<(), JournalError> {
        if self.poisoned {
            Err(JournalError::Poisoned)
        } else {
            Ok(())
        }
    }

    /// Append `records` at [`next_index`](Self::next_index) and return the
    /// indexes they got. On `Ok` every record is durable.
    ///
    /// Records are written in batches: the entries in one write, their slots
    /// in another, then one `fdatasync` per batch. A segment that runs out of
    /// slots or data space rolls over to a new one.
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
            let fit = self
                .tail()
                .fitting(&sizes[done..], self.config.max_batch_bytes);
            if fit == 0 {
                self.guarded_rollover().await?;
                continue;
            }
            let batch: Vec<(u64, &[u8])> = records[done..done + fit]
                .iter()
                .map(|record| (record.epoch, record.payload))
                .collect();
            let tail = self.tail_mut();
            let outcome = tail.append(&batch).await;
            if outcome.is_err() {
                self.poisoned = true;
            }
            outcome?;
            done += fit;
        }
        Ok(first..self.next_index())
    }

    async fn guarded_rollover(&mut self) -> Result<(), JournalError> {
        let outcome = self.rollover().await;
        if outcome.is_err() {
            self.poisoned = true;
        }
        outcome
    }

    /// Start a new segment at the next index. Every batch in the current one
    /// is already synced, so the manifest may name the successor as soon as
    /// the successor itself is durable.
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
        self.publish(self.segments.len(), None).await?;
        tracing::debug!(first, "journal segment rolled over");
        Ok(())
    }

    /// Discard every entry at `from` and after (Raft suffix truncation).
    ///
    /// The cut is first recorded in the manifest as a *fence*; then the
    /// discarded slots and entries are zeroed and synced; then the fence is
    /// lifted. The appends that follow can therefore never be mistaken for,
    /// or mixed up with, what was discarded — and a crash part-way, which may
    /// leave any mix of zeroed and surviving sectors, recovers to exactly
    /// `from` (fence recorded) or the old log (fence not yet durable).
    ///
    /// Cutting at the first index of a batch never rewrites a block that
    /// holds kept entries, because every batch starts on a block boundary.
    /// Cutting inside a batch zeroes the tail of the block where the cut
    /// falls, rewriting the kept entries that share it; a crash during that
    /// rewrite can tear them, and recovery then reports them in
    /// [`Recovery::ambiguous_tail`].
    ///
    /// # Errors
    ///
    /// [`JournalError::OutOfRange`] unless `start <= from <= next`,
    /// [`JournalError::Poisoned`], and any I/O error (which poisons).
    #[instrument(skip(self), fields(dir = %self.dir))]
    pub async fn truncate_suffix(&mut self, from: u64) -> Result<(), JournalError> {
        self.check_writable()?;
        let next = self.next_index();
        if from < self.start || from > next {
            return Err(JournalError::OutOfRange {
                index: from,
                start: self.start,
                next,
            });
        }
        if from == next {
            return Ok(());
        }
        let keep = self
            .segments
            .partition_point(|segment| segment.first <= from);
        let outcome = async {
            // Zeroing is many writes, and a crash resolves each of them on
            // its own: half-zeroed slots beside surviving entries would look
            // like mid-log damage. So the cut is made durable first, as a
            // fence recovery honours whatever state the zeroing reached.
            self.publish(keep, Some(from)).await?;
            if keep < self.segments.len() {
                for dropped in self.segments.drain(keep..) {
                    let path = segment_path(&self.dir, dropped.first);
                    drop(dropped);
                    self.provider.delete(&path).await?;
                }
                self.provider.sync_dir(&self.dir).await?;
            }
            self.tail_mut().truncate_from(from).await?;
            // The discarded range is zero and durable: lift the fence before
            // anything is appended at `from` again.
            self.publish(self.segments.len(), None).await
        }
        .await;
        if outcome.is_err() {
            self.poisoned = true;
        }
        outcome
    }

    /// Forget every entry before `before` (compaction). Whole segments below
    /// it are deleted; the live start is recorded durably either way.
    ///
    /// # Errors
    ///
    /// [`JournalError::OutOfRange`] unless `start <= before <= next`,
    /// [`JournalError::Poisoned`], and any I/O error (which poisons).
    #[instrument(skip(self), fields(dir = %self.dir))]
    pub async fn truncate_prefix(&mut self, before: u64) -> Result<(), JournalError> {
        self.check_writable()?;
        let next = self.next_index();
        if before < self.start || before > next {
            return Err(JournalError::OutOfRange {
                index: before,
                start: self.start,
                next,
            });
        }
        if before == self.start {
            return Ok(());
        }
        let drop_count = self
            .segments
            .partition_point(|segment| segment.first <= before)
            - 1;
        let outcome = async {
            let manifest = Manifest {
                start: before,
                fence: None,
                firsts: self.segments[drop_count..]
                    .iter()
                    .map(|s| s.first)
                    .collect(),
            };
            self.manifest
                .store(&self.provider, &manifest.encode())
                .await?;
            self.start = before;
            for dropped in self.segments.drain(..drop_count) {
                let path = segment_path(&self.dir, dropped.first);
                drop(dropped);
                self.provider.delete(&path).await?;
            }
            if drop_count > 0 {
                self.provider.sync_dir(&self.dir).await?;
            }
            Ok(())
        }
        .await;
        if outcome.is_err() {
            self.poisoned = true;
        }
        outcome
    }

    /// The caller's metadata (for example Raft's term and vote), as last
    /// saved.
    #[must_use]
    pub fn meta(&self) -> Option<&[u8]> {
        self.meta_value.as_deref()
    }

    /// Durably replace the caller's metadata. It lives in its own two-copy
    /// file (`meta.0`, `meta.1`), each copy with a generation and a CRC,
    /// updated through a temporary file, a sync, and a rename.
    ///
    /// # Errors
    ///
    /// Any I/O error; the previous value is still intact on disk.
    #[instrument(skip_all, fields(dir = %self.dir, len = bytes.len()))]
    pub async fn save_meta(&mut self, bytes: &[u8]) -> Result<(), JournalError> {
        self.meta.store(&self.provider, bytes).await?;
        self.meta_value = Some(bytes.to_vec());
        Ok(())
    }
}
