//! [`Journal`]: create, open, commit, read.

use std::collections::BTreeMap;
use std::ops::RangeBounds;

use moonpool_core::{AlignedBuf, DirectIo, LayoutRegion, LocalStorageFile, LocalStorageProvider};

use crate::batch::{Batch, Write};
use crate::format::{
    BLOCK, BLOCK_U64, EntryHeader, FLAG_ORDERED, Geometry, Id, Kind, META_COPY_B_AT, META_MAX,
    Meta, RECORD_SIZE, Record, SegmentHeader, align_up, clear_id, entry_crc, first_batch_of,
    generation_of, journal_tag, u64_of, usize_of,
};
use crate::io::block_buf;
use crate::meta::{META_NAME, MetaFile, remove_leftover};
use crate::recover::{Located, Op, Stale, entry_matches, scan};
use crate::segment::{Segment, is_segment_leftover, named_batches, parse_segment_name};
use crate::spare::{is_spare, spare_name};
use crate::{CommitError, Durable, OpenError, ReadError};

/// A journal's identity, stamped in its meta file, its segment headers and
/// (as a 32-bit tag) every entry, so files of another journal are refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JournalId(pub u128);

/// How many syncs a commit spends, and what that buys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Durability {
    /// CLSTORE's protocol: `write(entries); write(persist records); fsync`.
    /// One sync per batch. A crash can leave an intact persist record beside
    /// a damaged entry, exactly as corruption would, and the paper proves
    /// the two cannot be told apart for the last batch: its damaged entries
    /// are reported [`State::Ambiguous`].
    Batched,
    /// `write(entries); fsync; write(persist records); fsync`, issued as
    /// two durable writes ([`LocalStorageFile::write_at_durable`]: one
    /// `pwritev2(RWF_DSYNC)` each on Linux). Two syncs per batch; a persist
    /// record proves its entry was synced, so damage is always told apart.
    ///
    /// [`LocalStorageFile::write_at_durable`]: moonpool_core::LocalStorageFile::write_at_durable
    #[default]
    Ordered,
}

/// How a journal lays out and writes its files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JournalConfig {
    /// The protocol of each commit. Recorded per batch, so it may change
    /// between opens.
    pub durability: Durability,
    /// The segment shape: fixed at creation, checked at every open.
    pub geometry: Geometry,
    /// Direct-I/O policy for the segment files.
    pub direct_io: DirectIo,
}

/// What a position holds, as far as the journal knows without reading it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Nothing: never written, cleared, or below the floor.
    Empty,
    /// An entry, believed intact (read it to check).
    Live {
        /// Its identity.
        id: Id,
        /// Its payload length.
        len: u32,
    },
    /// The entry's bytes are damaged and its persist record proves it was
    /// written: corruption. Fetch it again from a peer.
    Corrupt {
        /// Its identity, from the persist record.
        id: Id,
    },
    /// The last batch of a [`Durability::Batched`] journal: a damaged entry
    /// beside an intact persist record is a crash or corruption, and nothing
    /// local tells which. Leave it (it becomes [`State::Corrupt`] once
    /// another batch commits) or clear it.
    Ambiguous {
        /// Its identity, from the persist record.
        id: Id,
    },
}

/// An entry read back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its position.
    pub position: u64,
    /// Its identity.
    pub id: Id,
    /// The caller's bytes.
    pub payload: Vec<u8>,
}

/// What opening found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recovery {
    /// Entries of the last batch whose bytes are damaged although their
    /// persist record proves them written (the rest of the log is checked
    /// when read).
    pub corrupt: Vec<(u64, Id)>,
    /// The last batch's damaged entries of a [`Durability::Batched`] commit.
    pub ambiguous: Vec<(u64, Id)>,
    /// Records of the last batch torn by a crash and discarded: both the
    /// record and its entry damaged.
    pub torn: u32,
    /// Persist records rebuilt from their intact entries.
    pub rebuilt: u32,
    /// A metainfo copy was rewritten from its twin.
    pub meta_repaired: bool,
    /// Segment header copies rewritten from their twin.
    pub headers_repaired: u32,
    /// Segments that held nothing live: kept as the spare or deleted.
    pub segments_removed: u32,
    /// Spare files deleted: one more than the spare kept, not a whole
    /// segment, or a second name a crash left on a live segment.
    pub spares_removed: u32,
    /// A dead segment's file an earlier open recycled, kept for the next
    /// roll.
    pub spare_kept: bool,
}

/// How this open of the journal rolled onto new segments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rolls {
    /// Rolls onto a spare, a dead segment's file: a header write and a
    /// rename.
    pub onto_spare: u64,
    /// Rolls that created the segment: an allocation and a header write.
    pub created: u64,
}

/// Where a position's persist record and entry live, for aiming faults:
/// both regions carry the position as their stripe, the key every replica's
/// copy of that position shares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The persist record ([`Layout::RECORD`]).
    pub record: LayoutRegion,
    /// The entry, header and payload ([`Layout::ENTRY`]).
    pub entry: LayoutRegion,
}

impl Layout {
    /// The kind of a persist record's region.
    pub const RECORD: &'static str = "persist record";
    /// The kind of an entry's region.
    pub const ENTRY: &'static str = "entry";
    /// The kind of a metainfo copy's region (node-unique: no stripe).
    pub const META: &'static str = "metainfo";
    /// The kind of a segment header copy's region (no stripe).
    pub const HEADER: &'static str = "segment header";
}

/// A CLSTORE journal over any [`LocalStorageProvider`]: see the crate docs.
///
/// One codebase for every backend. Over a `Send` provider (Tokio, the
/// simulator) the journal and the futures of its methods are `Send`, so it
/// runs inside `tokio::spawn`; over a thread-per-core provider (compio) they
/// are not, and it runs on that runtime's thread.
pub struct Journal<P: LocalStorageProvider> {
    provider: P,
    dir: String,
    id: JournalId,
    tag: u32,
    config: JournalConfig,
    meta_file: MetaFile<P::File>,
    meta: Meta,
    segments: Vec<Segment<P::File>>,
    index: BTreeMap<u64, Located>,
    /// The last batch in the log (0: none).
    last_batch: u64,
    /// The batch whose damaged entries are still ambiguous: the last one at
    /// open, until the next batch commits.
    ambiguous_batch: Option<u64>,
    /// This open's generation: the high bits of every batch it writes.
    generation: u32,
    poisoned: bool,
    /// The name of the spare, a dead segment's file kept for the next roll.
    spare: Option<String>,
    /// Spares this open has named: the number in the next one's name.
    spares_named: u32,
    rolls: Rolls,
}

impl<P: LocalStorageProvider> std::fmt::Debug for Journal<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("dir", &self.dir)
            .field("id", &self.id)
            .field("floor", &self.meta.floor)
            .field("last_batch", &self.last_batch)
            .field("live", &self.index.len())
            .field("segments", &self.segments.len())
            .field("poisoned", &self.poisoned)
            .field("spare", &self.spare)
            .field("rolls", &self.rolls)
            .finish_non_exhaustive()
    }
}

fn check_config(config: &JournalConfig) -> Result<(), OpenError> {
    if config.geometry.is_valid() {
        Ok(())
    } else {
        Err(OpenError::InvalidConfig(
            "the geometry describes no usable segment",
        ))
    }
}

/// Every directory whose entries name a component of `dir`, from the root
/// down: `.`, `a`, `a/b` for `a/b/c`; `/`, `/a` for `/a/b`. A name created
/// by `create_dir_all` is lost in a crash unless its parent is synced, and a
/// synced child does not survive the loss of its parent's own name.
fn ancestors_of(dir: &str) -> Vec<String> {
    let mut current = if dir.starts_with('/') { "/" } else { "." }.to_string();
    let mut parents = Vec::new();
    for component in dir.split('/').filter(|c| !c.is_empty() && *c != ".") {
        parents.push(current.clone());
        current = match current.as_str() {
            "." => component.to_string(),
            "/" => format!("/{component}"),
            _ => format!("{current}/{component}"),
        };
    }
    parents
}

/// Make every name on the way to `dir`, and the names inside it, durable.
/// Opening runs it too: an earlier attempt may have created a name, or
/// renamed a file into place, and failed before its directory sync.
async fn sync_names<P: LocalStorageProvider>(provider: &P, dir: &str) -> std::io::Result<()> {
    for parent in ancestors_of(dir) {
        provider.sync_dir(&parent).await?;
    }
    provider.sync_dir(dir).await
}

/// What a directory holds: its segments sorted by first batch and its
/// spares, once what a crashed creation left is removed.
struct Listing {
    firsts: Vec<(u64, String)>,
    spares: Vec<String>,
}

/// The segments and spares in `dir`, after removing what a crashed creation
/// left; `None` when `dir` does not exist.
async fn list_segments<P: LocalStorageProvider>(
    provider: &P,
    dir: &str,
) -> Result<Option<Listing>, OpenError> {
    let names = match provider.list_dir(dir).await {
        Ok(names) => names,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    sync_names(provider, dir).await?;
    remove_leftover(provider, dir).await?;
    let mut firsts: Vec<(u64, String)> = Vec::new();
    let mut spares = Vec::new();
    for name in names {
        if is_spare(&name) {
            spares.push(name);
        } else if is_segment_leftover(&name) {
            delete_if_present(provider, &format!("{dir}/{name}")).await?;
        } else if let Some(first) = parse_segment_name(&name) {
            firsts.push((first, name));
        }
    }
    firsts.sort();
    spares.sort();
    Ok(Some(Listing { firsts, spares }))
}

/// Keep one whole spare of `spares` and delete the others (with their
/// count). A spare is never evidence: no batch is written to it before it
/// is renamed to a segment, so deleting one loses nothing. Kept, its old
/// batches are older than any batch it will hold (see [`crate::spare`]).
///
/// A spare whose header names a segment of `firsts` is never kept: a crash
/// in a roll left it as a second name of that live segment.
async fn keep_one_spare<P: LocalStorageProvider>(
    provider: &P,
    dir: &str,
    spares: Vec<String>,
    firsts: &[(u64, String)],
    (journal, config): (JournalId, &JournalConfig),
) -> Result<(Option<String>, u32), OpenError> {
    let mut kept = None;
    let mut removed = 0;
    for name in spares {
        let path = format!("{dir}/{name}");
        let whole = kept.is_none() && {
            let file = provider
                .open(&path, moonpool_core::OpenOptions::read_only())
                .await?;
            file.size().await? == config.geometry.total_blocks() * BLOCK_U64
        };
        let aliased = whole
            && named_batches(provider, &path, journal, config.direct_io)
                .await?
                .iter()
                .any(|batch| firsts.iter().any(|(first, _)| first == batch));
        if whole && !aliased {
            kept = Some(name);
        } else {
            delete_if_present(provider, &path).await?;
            removed += 1;
        }
    }
    if removed > 0 {
        provider.sync_dir(dir).await?;
    }
    Ok((kept, removed))
}

/// Delete `path`, which the listing found: gone already is deleted.
async fn delete_if_present<P: LocalStorageProvider>(
    provider: &P,
    path: &str,
) -> std::io::Result<()> {
    match provider.delete(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        deleted => deleted,
    }
}

/// Make what this open found the log from now on: a new generation, durable
/// in both metainfo copies before any write, so whatever lies beyond the end
/// and was invisible here can never come back as evidence.
async fn begin_generation<F: moonpool_core::LocalStorageFile>(
    meta_file: &MetaFile<F>,
    meta: Meta,
    last_batch: u64,
) -> Result<Meta, OpenError> {
    let generation = meta
        .open_generation
        .checked_add(1)
        .ok_or(OpenError::InvalidConfig("open generations exhausted"))?;
    let meta = Meta {
        seq: meta.seq + 1,
        last_batch,
        open_generation: generation,
        open_last: last_batch,
        ..meta
    };
    meta_file.update(&meta).await?;
    Ok(meta)
}

impl<P: LocalStorageProvider> Journal<P> {
    /// Create a journal in `dir` with the caller's first metainfo.
    ///
    /// # Errors
    /// [`OpenError::AlreadyExists`] if `dir` holds a journal; any I/O error.
    pub async fn create(
        provider: P,
        dir: &str,
        id: JournalId,
        config: JournalConfig,
        meta: &[u8],
    ) -> Result<Self, OpenError> {
        check_config(&config)?;
        if meta.len() > META_MAX {
            return Err(OpenError::InvalidConfig(
                "the first metainfo exceeds META_MAX",
            ));
        }
        provider.create_dir_all(dir).await?;
        sync_names(&provider, dir).await?;
        if provider.exists(&format!("{dir}/{META_NAME}")).await? {
            return Err(OpenError::AlreadyExists);
        }
        let first = Meta {
            seq: 1,
            journal: id,
            floor: 0,
            last_batch: 0,
            open_generation: 1,
            open_last: 0,
            geometry: config.geometry,
            bytes: meta.to_vec(),
        };
        let meta_file = MetaFile::create(&provider, dir, &first).await?;
        Ok(Self {
            tag: journal_tag(id),
            provider,
            dir: dir.to_string(),
            id,
            config,
            meta_file,
            meta: first,
            segments: Vec::new(),
            index: BTreeMap::new(),
            last_batch: 0,
            ambiguous_batch: None,
            generation: 1,
            poisoned: false,
            spare: None,
            spares_named: 0,
            rolls: Rolls::default(),
        })
    }

    /// Open the journal in `dir` and recover it. `Ok(None)`: there is no
    /// journal here (no meta file, no segments); whether that is a fresh
    /// node or a lost disk is the caller's to judge.
    ///
    /// Everything opening reports is durable before it returns: a process
    /// that restarted without a power loss reads its predecessor's unsynced
    /// writes back from the page cache, so opening syncs what it keeps.
    ///
    /// # Errors
    /// The evidence variants of [`OpenError`] when the journal cannot be
    /// trusted; [`OpenError::Io`] when the provider fails (open again).
    pub async fn open(
        provider: P,
        dir: &str,
        id: JournalId,
        config: JournalConfig,
    ) -> Result<Option<(Self, Recovery)>, OpenError> {
        check_config(&config)?;
        let Some(Listing { firsts, spares }) = list_segments(&provider, dir).await? else {
            return Ok(None);
        };
        let Some((meta_file, meta, meta_repaired)) = MetaFile::open(&provider, dir, id).await?
        else {
            // Segments without metainfo: the node-unique state is gone.
            return if firsts.is_empty() {
                Ok(None)
            } else {
                Err(OpenError::MetaLost)
            };
        };
        if meta.geometry != config.geometry {
            return Err(OpenError::InvalidConfig(
                "the geometry differs from the one the journal was created with",
            ));
        }
        let (spare, spares_removed) =
            keep_one_spare(&provider, dir, spares, &firsts, (id, &config)).await?;
        let mut recovery = Recovery {
            meta_repaired,
            spares_removed,
            spare_kept: spare.is_some(),
            ..Recovery::default()
        };
        let mut segments = Vec::with_capacity(firsts.len());
        for (first, name) in &firsts {
            let (segment, repaired) = Segment::open(
                &provider,
                dir,
                name,
                *first,
                id,
                config.geometry,
                config.direct_io,
            )
            .await?;
            recovery.headers_repaired += u32::from(repaired);
            segments.push(segment);
        }

        let tag = journal_tag(id);
        let stale = Stale {
            generation: meta.open_generation,
            last: meta.open_last,
        };
        let mut scanned = scan(&segments, tag, stale).await?;
        if scanned.last_batch < meta.last_batch {
            return Err(OpenError::LostBatch {
                batch: meta.last_batch,
            });
        }
        for (segment, (persist_next, entry_next)) in segments.iter_mut().zip(&scanned.cursors) {
            segment.persist_next = *persist_next;
            segment.entry_next = *entry_next;
        }
        if let Some((at, offset, bytes)) = scanned.rewrite.take() {
            // A rebuilt record says its entry was persisted, and the entry it
            // was rebuilt from may be a predecessor's unsynced write still in
            // the page cache: the entry reaches the disk first, as in a
            // commit. (A crash between a rewrite and the entries' sync left
            // records naming entries that never landed: corruption from a
            // crash alone.)
            segments[at].sync().await?;
            segments[at].write(offset, bytes).await?;
        }
        // Whatever opening keeps must survive the next power loss.
        for segment in &segments {
            segment.sync().await?;
        }
        meta_file.sync().await?;
        let meta = begin_generation(&meta_file, meta, scanned.last_batch).await?;
        let generation = meta.open_generation;

        recovery.corrupt = scanned.corrupt;
        recovery.ambiguous = scanned.ambiguous;
        recovery.torn = scanned.voided;
        recovery.rebuilt = scanned.rebuilt;
        let ambiguous_batch = (!recovery.ambiguous.is_empty()).then_some(scanned.last_batch);
        let mut journal = Self {
            tag,
            provider,
            dir: dir.to_string(),
            id,
            config,
            meta_file,
            meta,
            segments,
            index: BTreeMap::new(),
            last_batch: scanned.last_batch,
            ambiguous_batch,
            generation,
            poisoned: false,
            spare,
            spares_named: 0,
            rolls: Rolls::default(),
        };
        for op in scanned.ops {
            journal.apply(&op);
        }
        journal.drop_below_floor();
        recovery.segments_removed = journal.remove_dead_segments().await?;
        journal.assert_invariants();
        Ok(Some((journal, recovery)))
    }

    /// The caller's metainfo of the journal in `dir`, read without opening
    /// it: no recovery, no repair, no sync, nothing created, so a probe never
    /// changes what the next open finds. `Ok(None)` where there is no journal.
    ///
    /// # Errors
    /// [`OpenError::MetaLost`] when no copy is valid,
    /// [`OpenError::WrongJournal`], or the provider's error.
    pub async fn peek_meta(
        provider: &P,
        dir: &str,
        id: JournalId,
    ) -> Result<Option<Vec<u8>>, OpenError> {
        Ok(MetaFile::<P::File>::peek(provider, dir, id)
            .await?
            .map(|meta| meta.bytes))
    }

    // ---- what the journal knows, from memory ----

    /// The journal's identity.
    #[must_use]
    pub fn id(&self) -> JournalId {
        self.id
    }

    /// The caller's metainfo.
    #[must_use]
    pub fn meta(&self) -> &[u8] {
        &self.meta.bytes
    }

    /// Positions below this are gone.
    #[must_use]
    pub fn floor(&self) -> u64 {
        self.meta.floor
    }

    /// The number of the last batch in the log (0 before the first).
    #[must_use]
    pub fn last_batch(&self) -> u64 {
        self.last_batch
    }

    /// What `position` holds, without reading it.
    #[must_use]
    pub fn state(&self, position: u64) -> State {
        self.index
            .get(&position)
            .map_or(State::Empty, |at| self.state_of(at))
    }

    fn state_of(&self, at: &Located) -> State {
        if !at.damaged {
            State::Live {
                id: at.id,
                len: at.len,
            }
        } else if self.ambiguous_batch == Some(at.batch) && at.flags & FLAG_ORDERED == 0 {
            State::Ambiguous { id: at.id }
        } else {
            State::Corrupt { id: at.id }
        }
    }

    /// Every non-empty position in order, with its state.
    pub fn positions(&self) -> impl Iterator<Item = (u64, State)> + '_ {
        self.index
            .iter()
            .map(|(&position, at)| (position, self.state_of(at)))
    }

    /// The highest non-empty position.
    #[must_use]
    pub fn last_position(&self) -> Option<u64> {
        self.index.keys().next_back().copied()
    }

    /// Where `position`'s persist record and entry live, for aiming a
    /// storage fault at one of them. `None` when the position is empty.
    #[must_use]
    pub fn layout(&self, position: u64) -> Option<Layout> {
        let at = self.index.get(&position)?;
        let segment = &self.segments[self.segment_index(at.segment)];
        let path = format!("{}/{}", self.dir, segment.name);
        let offset = u64::from(at.offset);
        let region = |bytes, kind| LayoutRegion {
            path: path.clone(),
            bytes,
            kind,
            stripe: Some(position),
            shard: None,
        };
        Some(Layout {
            record: region(
                at.record_at..at.record_at + u64_of(RECORD_SIZE),
                Layout::RECORD,
            ),
            entry: region(
                offset..offset + u64_of(RECORD_SIZE) + u64::from(at.len),
                Layout::ENTRY,
            ),
        })
    }

    /// Every region of the journal a fault could hit: each live position's
    /// record and entry (striped by position), the two metainfo copies and
    /// every segment's two header copies (node-unique: no stripe).
    #[must_use]
    pub fn regions(&self) -> Vec<LayoutRegion> {
        let meta = format!("{}/{}", self.dir, META_NAME);
        let mut regions = vec![
            LayoutRegion {
                path: meta.clone(),
                bytes: 0..BLOCK_U64,
                kind: Layout::META,
                stripe: None,
                shard: None,
            },
            LayoutRegion {
                path: meta,
                bytes: META_COPY_B_AT..META_COPY_B_AT + BLOCK_U64,
                kind: Layout::META,
                stripe: None,
                shard: None,
            },
        ];
        for segment in &self.segments {
            let path = format!("{}/{}", self.dir, segment.name);
            for copy in 0..2 {
                regions.push(LayoutRegion {
                    path: path.clone(),
                    bytes: copy * BLOCK_U64..(copy + 1) * BLOCK_U64,
                    kind: Layout::HEADER,
                    stripe: None,
                    shard: None,
                });
            }
        }
        for layout in self.index.keys().filter_map(|&p| self.layout(p)) {
            regions.push(layout.record);
            regions.push(layout.entry);
        }
        regions
    }

    // ---- reads ----

    /// Read the entry at `position`, checked against its persist record:
    /// its CRC, and that it is exactly the entry the record names.
    ///
    /// # Errors
    /// [`ReadError::Damaged`] with the record's identity when the bytes are
    /// damaged (a medium error reads as damage); [`ReadError::Empty`].
    pub async fn read(&self, position: u64) -> Result<Entry, ReadError> {
        let at = self
            .index
            .get(&position)
            .ok_or(ReadError::Empty { position })?;
        if at.damaged {
            return Err(ReadError::Damaged {
                position,
                id: at.id,
            });
        }
        let segment = &self.segments[self.segment_index(at.segment)];
        let bytes = segment
            .read_at(u64::from(at.offset), entry_bytes(at))
            .await?;
        self.judge(position, at, &bytes)
    }

    /// The entry at `position` from its bytes `bytes`, read at `at`: checked
    /// against its persist record (its CRC, and that it is exactly the entry
    /// the record names).
    fn judge(&self, position: u64, at: &Located, bytes: &[u8]) -> Result<Entry, ReadError> {
        assert_eq!(bytes.len(), entry_bytes(at), "judged on its whole entry");
        let record = record_of(position, at);
        let intact = EntryHeader::parse(bytes).is_some_and(|(header, stored)| {
            header.tag == self.tag
                && entry_matches(&record, &header, stored)
                && entry_crc(&bytes[..RECORD_SIZE], &bytes[RECORD_SIZE..]) == stored
        });
        if !intact {
            return Err(ReadError::Damaged {
                position,
                id: at.id,
            });
        }
        Ok(Entry {
            position,
            id: at.id,
            payload: bytes[RECORD_SIZE..].to_vec(),
        })
    }

    /// Read every non-empty position in `range`, in order: a damaged entry
    /// is reported in place and the replay goes on. Entries that follow each
    /// other in one segment are read together, at most `REPLAY_READ` bytes
    /// per read, and each is judged as [`read`](Self::read) judges it.
    ///
    /// # Errors
    /// Only a provider failure that is not the medium's.
    ///
    /// # Panics
    /// If a read returns fewer bytes than asked: a provider bug.
    pub async fn replay(
        &self,
        range: impl RangeBounds<u64>,
    ) -> Result<Vec<(u64, Result<Entry, ReadError>)>, std::io::Error> {
        let located: Vec<(u64, Located)> =
            self.index.range(range).map(|(&p, &at)| (p, at)).collect();
        let mut out = Vec::with_capacity(located.len());
        let mut first = 0;
        while first < located.len() {
            let (position, at) = located[first];
            if at.damaged {
                out.push((
                    position,
                    Err(ReadError::Damaged {
                        position,
                        id: at.id,
                    }),
                ));
                first += 1;
                continue;
            }
            // A run: the entries that follow each other in one segment,
            // read with one read of at most `REPLAY_READ` bytes.
            let start = u64::from(at.offset);
            let mut end = start + u64_of(entry_bytes(&at));
            let mut stop = first + 1;
            while let Some(&(_, next)) = located.get(stop) {
                let next_start = u64::from(next.offset);
                let next_end = next_start + u64_of(entry_bytes(&next));
                if next.segment != at.segment
                    || next.damaged
                    || next_start < end
                    || next_start - end > REPLAY_GAP
                    || next_end - start > REPLAY_READ
                {
                    break;
                }
                end = next_end;
                stop += 1;
            }
            let segment = &self.segments[self.segment_index(at.segment)];
            let bytes = segment.read_at(start, usize_of(end - start)).await?;
            assert_eq!(u64_of(bytes.len()), end - start, "a run reads whole");
            for &(position, at) in &located[first..stop] {
                let from = usize_of(u64::from(at.offset) - start);
                let entry = &bytes[from..from + entry_bytes(&at)];
                out.push((position, self.judge(position, &at, entry)));
            }
            first = stop;
        }
        assert_eq!(out.len(), located.len(), "every position replayed once");
        Ok(out)
    }

    // ---- writes ----

    /// Make `batch` durable, then return. See [`Durability`] for the
    /// protocol. Metainfo, when the batch changes it, is written copy A in
    /// the first window and copy B in the second.
    ///
    /// # Errors
    /// [`CommitError::BelowFloor`], [`CommitError::MetaTooLarge`] and
    /// [`CommitError::BatchTooLarge`] before anything is written; an I/O
    /// failure poisons the journal ([`CommitError::Io`] says whether the
    /// batch may be durable anyway).
    pub async fn commit(&mut self, batch: Batch) -> Result<(), CommitError> {
        if self.poisoned {
            return Err(CommitError::Poisoned);
        }
        let floor = batch
            .floor
            .map_or(self.meta.floor, |f| f.max(self.meta.floor));
        for write in &batch.writes {
            if let Write::Put { position, .. } = write
                && *position < floor
            {
                return Err(CommitError::BelowFloor {
                    position: *position,
                    floor,
                });
            }
        }
        if let Some(meta) = &batch.meta
            && meta.len() > META_MAX
        {
            return Err(CommitError::MetaTooLarge { len: meta.len() });
        }
        if !batch.fits(self.config.geometry) {
            return Err(CommitError::BatchTooLarge);
        }
        let meta_changes = batch.meta.is_some() || floor != self.meta.floor;
        let next_meta = Meta {
            seq: self.meta.seq + 1,
            floor,
            last_batch: self.last_batch,
            bytes: batch
                .meta
                .clone()
                .unwrap_or_else(|| self.meta.bytes.clone()),
            ..self.meta.clone()
        };
        if batch.writes.is_empty() {
            if meta_changes {
                self.write_meta_only(&next_meta).await?;
                self.meta = next_meta;
                self.after_commit().await?;
            }
            return Ok(());
        }
        let mut prepared = self.prepare(&batch)?;
        let at = self.segment_for(&prepared).await?;
        let segment = &self.segments[at];
        prepared.place(segment.persist_next, segment.entry_next);
        let ordered = self.config.durability == Durability::Ordered;
        let meta = meta_changes.then_some(&next_meta);
        if let Err((error, durable)) =
            write_protocol(segment, &self.meta_file, &mut prepared, meta, ordered).await
        {
            self.poisoned = true;
            return Err(CommitError::Io { error, durable });
        }

        // Durable: now memory follows.
        let segment = &mut self.segments[at];
        segment.persist_next = prepared.persist_at + u64_of(prepared.records.len());
        segment.entry_next = prepared.entry_at + u64_of(prepared.entries.len());
        let first_batch = segment.first_batch;
        for (index, (record, write)) in (0u16..).zip(prepared.decoded.iter().zip(&batch.writes)) {
            let op = match write {
                Write::Put { position, .. } => Op::Put {
                    position: *position,
                    at: Located {
                        segment: first_batch,
                        record_at: prepared.persist_at + Record::slot_offset(index),
                        offset: record.offset,
                        len: record.len,
                        entry_crc: record.entry_crc,
                        id: record.id,
                        batch: record.batch,
                        index,
                        count: record.count,
                        flags: record.flags,
                        damaged: false,
                    },
                },
                Write::Clear { start, end } => Op::Clear {
                    start: *start,
                    end: *end,
                },
            };
            self.apply(&op);
        }
        self.last_batch = prepared.seq;
        self.ambiguous_batch = None;
        if meta_changes {
            self.meta = next_meta;
        }
        self.after_commit().await
    }

    /// The floor took effect and segments emptied: recycle or drop what is
    /// dead.
    async fn after_commit(&mut self) -> Result<(), CommitError> {
        self.drop_below_floor();
        if let Err(error) = self.remove_dead_segments().await {
            self.poisoned = true;
            return Err(CommitError::Io {
                error,
                durable: Durable::Unknown,
            });
        }
        self.assert_invariants();
        Ok(())
    }

    // ---- spare segments ----

    fn next_spare_name(&mut self) -> String {
        let name = spare_name(self.generation, self.spares_named);
        self.spares_named = self.spares_named.wrapping_add(1);
        name
    }

    /// How this open rolled onto new segments so far.
    #[must_use]
    pub fn rolls(&self) -> Rolls {
        self.rolls
    }

    fn prepare(&self, batch: &Batch) -> Result<Prepared, CommitError> {
        let seq = if generation_of(self.last_batch) == u64::from(self.generation) {
            self.last_batch + 1
        } else {
            first_batch_of(u64::from(self.generation))
        };
        assert!(seq > self.last_batch, "batch numbers only grow");
        let count = u16::try_from(batch.writes.len()).map_err(|_| CommitError::BatchTooLarge)?;
        let flags = match self.config.durability {
            Durability::Ordered => FLAG_ORDERED,
            Durability::Batched => 0,
        };
        // Entries are laid out relative to the batch's first entry; the
        // offsets are fixed once a segment is chosen. They are encoded in
        // place, in the aligned buffer the segment write moves to the
        // kernel: sized first (`Batch::fits` bounded it), so it is
        // allocated once, zeroed padding included, and never copied.
        let mut footprint = 0_u64;
        for write in &batch.writes {
            let len = match write {
                Write::Put { payload, .. } => payload.len(),
                Write::Clear { .. } => 0,
            };
            let len = u32::try_from(len).map_err(|_| CommitError::BatchTooLarge)?;
            footprint += EntryHeader::footprint(len);
        }
        let mut entries = block_buf(usize_of(align_up(footprint, BLOCK_U64)));
        let mut start = 0;
        let mut decoded = Vec::with_capacity(batch.writes.len());
        for (index, write) in (0u16..).zip(&batch.writes) {
            let (kind, position, id, payload): (Kind, u64, Id, &[u8]) = match write {
                Write::Put {
                    position,
                    id,
                    payload,
                } => (Kind::Put, *position, *id, payload),
                Write::Clear { start, end } => (Kind::Clear, *start, clear_id(*end), &[]),
            };
            let len = u32::try_from(payload.len()).map_err(|_| CommitError::BatchTooLarge)?;
            let header = EntryHeader {
                kind,
                flags,
                index,
                count,
                batch: seq,
                position,
                id,
                len,
                tag: self.tag,
            };
            let entry_crc = header.encode(payload, &mut entries[start..]);
            decoded.push(Record {
                kind,
                flags,
                index,
                count,
                batch: seq,
                position,
                id,
                offset: u32::try_from(start).map_err(|_| CommitError::BatchTooLarge)?,
                len,
                entry_crc,
            });
            start += usize_of(EntryHeader::footprint(len));
        }
        assert_eq!(
            u64_of(start),
            footprint,
            "the entries fill the footprint they were sized for"
        );
        let records = block_buf(usize_of(Record::blocks_for(decoded.len()) * BLOCK_U64));
        let geometry = self.config.geometry;
        // Pair with `Batch::fits`, checked before anything was laid out.
        assert!(
            Record::blocks_for(decoded.len()) <= u64::from(geometry.persist_blocks),
            "a batch that fits holds its records in one persist log"
        );
        assert!(
            u64_of(entries.len()) <= u64::from(geometry.entry_blocks) * BLOCK_U64,
            "a batch that fits holds its entries in one entry log"
        );
        Ok(Prepared {
            seq,
            entries,
            records,
            decoded,
            persist_at: 0,
            entry_at: 0,
        })
    }

    /// The segment this batch goes to, rolling over when the active one is
    /// full; fixes the batch's offsets.
    async fn segment_for(&mut self, prepared: &Prepared) -> Result<usize, CommitError> {
        let persist_blocks = u64_of(prepared.records.len()) / BLOCK_U64;
        let entry_bytes = u64_of(prepared.entries.len());
        let fits = self
            .segments
            .last()
            .is_some_and(|s| s.fits(persist_blocks, entry_bytes));
        if !fits {
            let spare = self.spare.take();
            let created = match &spare {
                Some(spare) => {
                    let header = SegmentHeader {
                        journal: self.id,
                        first_batch: prepared.seq,
                        prev_batch: self.last_batch,
                        geometry: self.config.geometry,
                    };
                    Segment::adopt(
                        &self.provider,
                        &self.dir,
                        spare,
                        header,
                        self.config.direct_io,
                    )
                    .await
                }
                None => {
                    Segment::create(
                        &self.provider,
                        &self.dir,
                        self.id,
                        prepared.seq,
                        self.last_batch,
                        self.config.geometry,
                        self.config.direct_io,
                    )
                    .await
                }
            };
            match created {
                Ok(segment) => {
                    assert_eq!(segment.first_batch, prepared.seq, "rolled for this batch");
                    self.segments.push(segment);
                    if spare.is_some() {
                        self.rolls.onto_spare += 1;
                    } else {
                        self.rolls.created += 1;
                    }
                }
                Err(error) => {
                    self.poisoned = true;
                    // The provider's own error keeps its kind, so a caller
                    // tells a full disk (`StorageFull`) from a failing one.
                    let error = match error {
                        OpenError::Io(error) => error,
                        other => std::io::Error::other(other.to_string()),
                    };
                    return Err(CommitError::Io {
                        error,
                        durable: Durable::No,
                    });
                }
            }
        }
        Ok(self.segments.len() - 1)
    }

    async fn write_meta_only(&mut self, meta: &Meta) -> Result<(), CommitError> {
        self.meta_file.update(meta).await.map_err(|error| {
            self.poisoned = true;
            CommitError::Io {
                error,
                durable: Durable::Unknown,
            }
        })
    }

    // ---- the index ----

    fn segment_index(&self, first_batch: u64) -> usize {
        self.segments
            .binary_search_by_key(&first_batch, |s| s.first_batch)
            .expect("a located entry's segment is open")
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::Put { position, at } => {
                let new = self.segment_index(at.segment);
                self.segments[new].live += 1;
                if let Some(old) = self.index.insert(*position, *at) {
                    self.release(&old);
                }
            }
            Op::Clear { start, end } => {
                let gone: Vec<u64> = self.index.range(*start..*end).map(|(&p, _)| p).collect();
                for position in gone {
                    let old = self.index.remove(&position).expect("listed");
                    self.release(&old);
                }
            }
        }
    }

    fn release(&mut self, old: &Located) {
        let at = self.segment_index(old.segment);
        assert!(
            self.segments[at].live > 0,
            "a segment's live count covers its entries"
        );
        self.segments[at].live -= 1;
    }

    fn drop_below_floor(&mut self) {
        let floor = self.meta.floor;
        let gone: Vec<u64> = self.index.range(..floor).map(|(&p, _)| p).collect();
        for position in gone {
            let old = self.index.remove(&position).expect("listed");
            self.release(&old);
        }
    }

    /// Remove the leading segments that hold nothing live, never the active
    /// one, and only from the front: a later segment may hold the write or
    /// the tombstone that makes an earlier one dead, so it must outlive it.
    /// The first becomes the spare when there is none (renamed, never
    /// written: see [`crate::spare`]); the others are deleted. Each rename
    /// or unlink is made durable before the next, so a crash can only bring
    /// back the front one, never leave a hole in the chain, and a spare is
    /// never written while it may still come back as its old segment.
    async fn remove_dead_segments(&mut self) -> std::io::Result<u32> {
        let mut removed = 0;
        while self.segments.len() > 1 && self.segments[0].live == 0 {
            let path = format!("{}/{}", self.dir, self.segments[0].name);
            if self.spare.is_none() {
                let name = self.next_spare_name();
                self.provider
                    .rename(&path, &format!("{}/{name}", self.dir))
                    .await?;
                self.provider.sync_dir(&self.dir).await?;
                self.spare = Some(name);
            } else {
                self.provider.delete(&path).await?;
                self.provider.sync_dir(&self.dir).await?;
            }
            drop(self.segments.remove(0));
            removed += 1;
        }
        Ok(removed)
    }

    fn assert_invariants(&self) {
        let live: u64 = self.segments.iter().map(|s| s.live).sum();
        assert_eq!(
            live,
            u64_of(self.index.len()),
            "live counts cover the index"
        );
        assert!(
            self.index
                .keys()
                .next()
                .is_none_or(|&p| p >= self.meta.floor),
            "nothing live below the floor"
        );
        assert!(
            self.segments
                .windows(2)
                .all(|w| w[0].first_batch < w[1].first_batch),
            "segments are ordered by their first batch"
        );
        assert!(
            self.meta.bytes.len() <= META_MAX && BLOCK == 4096,
            "metainfo fits its block"
        );
    }
}

/// The persist record a located entry was written under.
/// The most bytes one replay read covers: a run of entries longer than
/// this is read in several.
const REPLAY_READ: u64 = 1 << 20;

/// The most bytes between two entries a replay read covers rather than
/// starting a new read: the alignment after a batch, or entries since
/// released.
const REPLAY_GAP: u64 = 64 * BLOCK_U64;

const _: () = assert!(REPLAY_GAP < REPLAY_READ);

/// The bytes of the entry at `at`: its header and its payload.
fn entry_bytes(at: &Located) -> usize {
    RECORD_SIZE + usize_of(u64::from(at.len))
}

fn record_of(position: u64, at: &Located) -> Record {
    Record {
        kind: Kind::Put,
        flags: at.flags,
        index: at.index,
        count: at.count,
        batch: at.batch,
        position,
        id: at.id,
        offset: at.offset,
        len: at.len,
        entry_crc: at.entry_crc,
    }
}

/// A batch encoded: its entries (offsets relative to the batch's first
/// entry until [`Prepared::place`]) and its persist records.
struct Prepared {
    seq: u64,
    /// The entry log's bytes, block-padded, in the buffer the write moves.
    entries: AlignedBuf,
    /// The persist log's bytes, block-padded, likewise.
    records: AlignedBuf,
    decoded: Vec<Record>,
    persist_at: u64,
    entry_at: u64,
}

impl Prepared {
    /// Fix the batch at its place in a segment and encode its records.
    fn place(&mut self, persist_at: u64, entry_at: u64) {
        assert!(
            persist_at.is_multiple_of(BLOCK_U64) && entry_at.is_multiple_of(BLOCK_U64),
            "a batch starts on a fresh block in both logs"
        );
        self.persist_at = persist_at;
        self.entry_at = entry_at;
        for (slot, record) in self.decoded.iter_mut().enumerate() {
            record.offset = u32::try_from(entry_at + u64::from(record.offset))
                .expect("geometry bounds offsets to u32");
            record.encode(&mut self.records[slot * RECORD_SIZE..]);
        }
    }
}

/// How often an active commit point fires per commit. A commit is short
/// and a cut inside it is the case its recovery exists for, so the rate is
/// the buggify default rather than the lower rate of a hot-path point.
const COMMIT_HINT_PROB: f64 = 0.25;

/// The commit protocol (see [`Durability`]), then the metainfo: copy A only
/// once the batch is synced, copy B once copy A is. A durable metainfo
/// therefore always vouches for a durable batch; the reverse (the batch
/// durable, its metainfo not) is what a crash between the two leaves.
///
/// The batch's two buffers move into their writes and back (no copy); a
/// failed write drops its buffer, which a poisoned journal never reads.
async fn write_protocol<F: moonpool_core::LocalStorageFile>(
    segment: &Segment<F>,
    meta_file: &MetaFile<F>,
    batch: &mut Prepared,
    meta: Option<&Meta>,
    ordered: bool,
) -> Result<(), (std::io::Error, Durable)> {
    let unknown = |error| (error, Durable::Unknown);
    let entries = std::mem::take(&mut batch.entries);
    let records = std::mem::take(&mut batch.records);
    if ordered {
        // Each log's write is durable on return, entries first: two durable
        // writes where a write and a sync each were four calls. A durable
        // write covers its own blocks only, which is the whole barrier here:
        // every earlier write to this segment is durable (each commit's
        // writes are before it returns, open syncs every segment, a roll's
        // headers are written durably, and a failed commit poisons the
        // journal), and data-only durability suffices because a segment
        // never changes size. The windows inside each write (bytes landed,
        // not yet durable) are the storage layer's to name.
        batch.entries = segment
            .write_durable(batch.entry_at, entries)
            .await
            .map_err(|error| (error, Durable::No))?;
        batch.records = segment
            .write_durable(batch.persist_at, records)
            .await
            .map_err(unknown)?;
    } else {
        // One barrier for both logs: it must cover the entries too, which a
        // durable write of the records alone would not.
        batch.entries = segment
            .write(batch.entry_at, entries)
            .await
            .map_err(|error| (error, Durable::No))?;
        // A power loss here tears entries no record points at yet.
        moonpool_buggify::hint!(
            "journal commit: entries written, not synced",
            COMMIT_HINT_PROB
        )
        .await;
        batch.records = segment
            .write(batch.persist_at, records)
            .await
            .map_err(unknown)?;
        // The commit's window: a power loss here may tear the records,
        // rebuild them from their entries, or leave the last batch
        // ambiguous.
        moonpool_buggify::hint!(
            "journal commit: records written, not synced",
            COMMIT_HINT_PROB
        )
        .await;
        sync_batch(segment, batch).await.map_err(unknown)?;
    }
    if let Some(meta) = meta {
        // The batch is durable and the metainfo still names the old one.
        moonpool_buggify::hint!(
            "journal commit: batch synced, metainfo stale",
            COMMIT_HINT_PROB
        )
        .await;
        meta_file.update(meta).await.map_err(unknown)?;
    }
    Ok(())
}

/// How many syncs a [`Durability::Batched`] commit tries before it fails.
const SYNC_ATTEMPTS: u32 = 3;

/// Sync a [`Durability::Batched`] batch, writing it again before each retry.
///
/// A failed sync can leave pages clean in the page cache but not on the
/// device (fsyncgate): a later sync reports success, and reads keep serving
/// the bytes until the cache drops them. One sync covers the entries and the
/// records, so the device can keep a record whose entry it lost: a damaged
/// batch that no crash made. The batch is not acknowledged yet, so writing
/// its own bytes again is safe: it dirties every page it holds, and the
/// next sync flushes them. A power loss during the retry can tear the batch,
/// which a power loss before the first sync could already do. Ordered
/// commits need no retry: each durable write covers its own blocks.
async fn sync_batch<F: moonpool_core::LocalStorageFile>(
    segment: &Segment<F>,
    batch: &mut Prepared,
) -> std::io::Result<()> {
    let mut attempt = 1;
    loop {
        let Err(error) = segment.sync().await else {
            return Ok(());
        };
        if attempt == SYNC_ATTEMPTS {
            return Err(error);
        }
        attempt += 1;
        let entries = std::mem::take(&mut batch.entries);
        batch.entries = segment.write(batch.entry_at, entries).await?;
        let records = std::mem::take(&mut batch.records);
        batch.records = segment.write(batch.persist_at, records).await?;
        // A power loss here tears a batch whose first sync failed.
        moonpool_buggify::hint!(
            "journal commit: batch written again, not synced",
            COMMIT_HINT_PROB
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::ancestors_of;

    #[test]
    fn every_ancestor_from_the_root_down() {
        assert_eq!(ancestors_of("wal"), vec!["."]);
        assert_eq!(ancestors_of("a/b/c"), vec![".", "a", "a/b"]);
        assert_eq!(ancestors_of("/a/b"), vec!["/", "/a"]);
    }
}
