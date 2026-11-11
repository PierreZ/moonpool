//! Journal example: `moonpool-journal` under crash attrition and every
//! storage fault a disk can make.
//!
//! A single [`JournalNode`] owns a CLSTORE journal on its simulated disk. On
//! every boot it opens the journal (running the recovery scan), checks what
//! it reports against the ledger of acknowledged writes, and then commits
//! batches until attrition crashes it again: entries at positions in any
//! order (a lagging replica's fill-in), overwrites, tombstones, floor raises
//! and metainfo. Each boot draws its [`Durability`], so one run proves both
//! protocols and a journal that changes protocol between opens.
//!
//! The disk runs `Chaos::Storage(Random)` with every family but a failed
//! disk: crash physics, lost directory entries, failed syncs, short
//! transfers, and also rot, EIO, misdirected and phantom writes, which
//! damage data a lone node has no second copy of. A [`JournalStrike`]
//! injector adds aimed double faults: it rots a persist record and its entry
//! in the last acknowledged batch (or every record and the first entry of
//! it), then crashes the node.
//!
//! A lone node cannot repair such damage, so the check is not "nothing is
//! lost". It is the **reporting oracle**: the journal never hides damage.
//! For each acknowledged `(position, id)`, after every open, one of these
//! holds:
//!
//! - the replay returns the exact payload;
//! - the replay returns `ReadError::Damaged` with that position and that id;
//! - the open refuses with an evidence variant of `OpenError`.
//!
//! The journal must never return other bytes, an older id as the current
//! one, or nothing. For a write that was not acknowledged, absent, old and
//! new are all correct; a mixed payload is not. One case is allowed to
//! forget: the last batch, when the metainfo does not record it (no metainfo
//! change in its commit and no open since). A crash in the middle of its
//! commit leaves the same bytes as two faults on it, so the journal voids
//! it, and its positions may read as they were before it.
//!
//! What the node does with a report is a replication layer's job, played
//! from the ledger as a peer would: a damaged acknowledged entry is written
//! again, a damaged entry that was never acknowledged is cleared, and a
//! refused open makes the node rebuild its journal from the ledger.
//!
//! A failed sync is an operating error, not a verdict: the batch it covered
//! was never acknowledged. The node does not reopen the journal in place,
//! though. After a failed writeback (fsyncgate) the page cache still holds
//! bytes the disk never got, a retried sync reports success for them, and a
//! reopen would build on them. So the node crashes itself, as PostgreSQL
//! panics, and the reboot recovers from what the disk really holds.
//!
//! The ledger lives in the iteration's [`StateHandle`](moonpool_sim::StateHandle),
//! so it survives the reboots that the node's memory does not.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_journal::{
    Batch, CommitError, Durability, Durable, Geometry, ID_SIZE, Id, Journal, JournalConfig,
    JournalId, OpenError, Recovery,
};
use moonpool_journal::{Layout, ReadError};
use moonpool_sim::{
    FaultContext, FaultInjector, Process, RandomProvider, RebootKind, SimContext,
    SimStorageProvider, SimulationResult, StorageProvider, TimeProvider, Workload, assert_always,
    assert_reachable, assert_sometimes,
};

use crate::support::{invalid_state, unless_shutdown};

/// The journal's directory on the node's disk.
const DIR: &str = "wal";

/// The journal's identity.
const ID: JournalId = JournalId(0x006A_6F75_726E_616C);

/// The [`StateHandle`](moonpool_sim::StateHandle) key the ledger lives under.
const LEDGER: &str = "journal_ledger";

/// How long the workload keeps the run going; attrition stops at the
/// builder's `chaos_duration`, and the rest is a quiet tail.
pub const RUN_FOR: Duration = Duration::from_secs(40);

/// Positions are drawn in this window above the floor, in any order.
const WINDOW: u64 = 96;

/// The small geometry: 512 records and 64 KiB of entries per segment, so a
/// run rolls over many segments and the floor frees them.
fn config(durability: Durability) -> JournalConfig {
    JournalConfig {
        durability,
        geometry: Geometry::small(),
        ..JournalConfig::default()
    }
}

/// A value at a position.
type Value = (Id, Vec<u8>);

/// The batch in flight: any part of it may survive a crash.
#[derive(Clone, Default)]
struct Pending {
    puts: Vec<(u64, Value)>,
    clears: Vec<Range<u64>>,
    floor: Option<u64>,
    meta: Option<Vec<u8>>,
    /// The commit failed with `Durable::No`: the journal promised that
    /// nothing of it reached the disk.
    known_absent: bool,
}

impl Pending {
    fn touches(&self, position: u64) -> bool {
        self.puts.iter().any(|(p, _)| *p == position)
            || self.clears.iter().any(|r| r.contains(&position))
            || self.floor.is_some_and(|f| position < f)
    }

    /// The batch that commits it.
    fn batch(&self) -> Batch {
        let mut batch = Batch::new();
        for (position, (id, payload)) in &self.puts {
            batch.put(*position, *id, payload.clone());
        }
        for range in &self.clears {
            batch.clear(range.clone());
        }
        if let Some(floor) = self.floor {
            batch.truncate_prefix(floor);
        }
        if let Some(meta) = &self.meta {
            batch.set_meta(meta.clone());
        }
        batch
    }

    fn put_at(&self, position: u64) -> impl Iterator<Item = &Value> {
        self.puts
            .iter()
            .filter(move |(p, _)| *p == position)
            .map(|(_, v)| v)
    }
}

/// The last acknowledged batch, as far as the oracle needs it.
#[derive(Clone, Default)]
struct LastBatch {
    /// The metainfo records it (its commit changed the metainfo, or an
    /// open found it last): two faults on it are evidence, never a crash.
    witnessed: bool,
    /// What each position it touched held before it: where it may read
    /// when the batch is voided.
    before: BTreeMap<u64, Option<Value>>,
}

impl LastBatch {
    /// Whether `position` may read as `value` (`None`: nothing) because the
    /// last batch was voided: as before it, or as nothing (the commit freed
    /// the segment that held the value before it).
    fn may_read_as(&self, position: u64, value: Option<&Value>) -> bool {
        !self.witnessed
            && self
                .before
                .get(&position)
                .is_some_and(|before| value.is_none() || before.as_ref() == value)
    }

    /// Whether `id` at `position` may be reported damaged because the last
    /// batch was voided and the entry before it is damaged.
    fn may_be_damaged_as(&self, position: u64, id: &Id) -> bool {
        !self.witnessed
            && self
                .before
                .get(&position)
                .is_some_and(|before| before.as_ref().is_some_and(|(i, _)| i == id))
    }
}

/// Where [`JournalStrike`] aims: the persist records and entries of the
/// last acknowledged batch.
#[derive(Clone)]
struct Target {
    /// Each put's record and entry, in the batch's order.
    puts: Vec<Layout>,
    /// The batch holds only puts, so `puts` names every record of it.
    puts_only: bool,
}

/// What the node has acknowledged, shared across its reboots.
#[derive(Clone, Default)]
pub struct Ledger(Arc<Mutex<LedgerState>>);

#[derive(Default)]
struct LedgerState {
    acked: BTreeMap<u64, Value>,
    floor: u64,
    meta: Vec<u8>,
    pending: Option<Pending>,
    last: LastBatch,
    target: Option<Target>,
    /// A rebuild from the ledger is under way: the disk holds part of it.
    rebuilding: bool,
    /// Commits so far: each one's version makes its values unique.
    version: u64,
}

impl LedgerState {
    /// An acknowledged batch: it is durable.
    fn ack(&mut self, pending: Pending, target: Target) {
        let mut before = BTreeMap::new();
        for (position, _) in &pending.puts {
            before.insert(*position, self.acked.get(position).cloned());
        }
        for range in &pending.clears {
            for (position, value) in self.acked.range(range.clone()) {
                before.insert(*position, Some(value.clone()));
            }
        }
        self.last = LastBatch {
            witnessed: pending.meta.is_some() || pending.floor.is_some(),
            before,
        };
        self.target = Some(target);
        for (position, value) in pending.puts {
            self.acked.insert(position, value);
        }
        for range in pending.clears {
            self.acked.retain(|p, _| !range.contains(p));
        }
        if let Some(floor) = pending.floor {
            self.floor = self.floor.max(floor);
            assert_reachable!("the node raised its floor");
        }
        if let Some(meta) = pending.meta {
            self.meta = meta;
        }
        let floor = self.floor;
        self.acked.retain(|p, _| *p >= floor);
        self.pending = None;
    }
}

impl Ledger {
    /// The iteration's ledger, published on first use.
    fn shared(state: &moonpool_sim::StateHandle) -> Self {
        if let Some(ledger) = state.get::<Self>(LEDGER) {
            return ledger;
        }
        let ledger = Self::default();
        state.publish(LEDGER, ledger.clone());
        ledger
    }

    fn with<T>(&self, f: impl FnOnce(&mut LedgerState) -> T) -> T {
        f(&mut self
            .0
            .lock()
            .expect("ledger lock poisoned: prior task panicked"))
    }

    /// How many positions hold an acknowledged value.
    #[must_use]
    pub fn acked_len(&self) -> usize {
        self.with(|state| state.acked.len())
    }
}

/// Why one opening of the journal ended.
#[derive(Debug)]
enum Ended {
    Open(OpenError),
    Commit(CommitError),
}

/// The node that owns the journal.
pub struct JournalNode;

#[async_trait]
impl Process for JournalNode {
    fn name(&self) -> &'static str {
        "journal_node"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let ledger = Ledger::shared(ctx.state());
        loop {
            let Some(life) = unless_shutdown(ctx, live(ctx, &ledger)).await else {
                return Ok(());
            };
            match life {
                // A failed sync (or another I/O error) poisons the journal.
                // A reopen would trust the page cache, which may hold bytes a
                // failed writeback never wrote: crash, and recover from disk.
                Ended::Open(OpenError::Io(_)) | Ended::Commit(CommitError::Io { .. }) => {
                    assert_reachable!("an I/O error made the node crash itself");
                    let delay = Duration::from_millis(ctx.random().random_range(10..200));
                    ctx.crash_self(RebootKind::Crash, Some(delay))?;
                    // The kill lands before the next storage completion.
                    std::future::pending::<()>().await;
                }
                Ended::Open(error) => {
                    return Err(invalid_state(format!("journal open failed: {error}")));
                }
                Ended::Commit(error) => {
                    return Err(invalid_state(format!("journal commit failed: {error}")));
                }
            }
        }
    }
}

/// Whether an open refused with evidence of damage: the journal reported
/// what it would not guess about.
fn is_evidence(error: &OpenError) -> bool {
    matches!(
        error,
        OpenError::DoubleFault { .. }
            | OpenError::LostBatch { .. }
            | OpenError::MetaLost
            | OpenError::BadSegment { .. }
            | OpenError::MetaRolledBack { .. }
    )
}

/// One opening of the journal: recover and check it, then write until an
/// error ends it.
async fn live(ctx: &SimContext, ledger: &Ledger) -> Ended {
    let durability = if ctx.random().random_bool(0.5) {
        Durability::Ordered
    } else {
        Durability::Batched
    };
    if ledger.with(|state| state.rebuilding) {
        return match rebuild(ctx, ledger, durability).await {
            Ok(journal) => run(ctx, journal, ledger).await,
            Err(ended) => ended,
        };
    }
    note_spares(ctx).await;
    let provider = ctx.storage().clone();
    let opened = match Journal::open(provider.clone(), DIR, ID, config(durability)).await {
        Ok(Some(found)) => Ok(found),
        Ok(None) => Journal::create(provider, DIR, ID, config(durability), b"")
            .await
            .map(|journal| (journal, Recovery::default())),
        Err(error) => Err(error),
    };
    let (mut journal, recovery) = match opened {
        Ok(found) => found,
        Err(error @ OpenError::Io(_)) => return Ended::Open(error),
        Err(error) if is_evidence(&error) => {
            assert_reachable!("an open refused on evidence of damage");
            if matches!(error, OpenError::DoubleFault { .. }) {
                assert_reachable!("an open refused a double fault");
            }
            return match rebuild(ctx, ledger, durability).await {
                Ok(journal) => run(ctx, journal, ledger).await,
                Err(ended) => ended,
            };
        }
        Err(error) => {
            assert_always!(false, "an open either recovers or refuses on evidence", {
                "error" => error
            });
            return Ended::Open(error);
        }
    };
    note_recovery(ctx, &recovery).await;
    if let Err(error) = reconcile(&mut journal, ledger).await {
        return Ended::Commit(error);
    }
    run(ctx, journal, ledger).await
}

/// Commit until an error ends it.
async fn run(ctx: &SimContext, mut journal: Journal<SimStorageProvider>, ledger: &Ledger) -> Ended {
    loop {
        if let Err(error) = step(ctx, &mut journal, ledger).await {
            return Ended::Commit(error);
        }
        let pause = Duration::from_millis(ctx.random().random_range(1..20));
        // A sleep only fails at shutdown, which the caller watches for.
        let _ = ctx.time().sleep(pause).await;
    }
}

/// The open refused: rebuild the journal from the ledger, as a node
/// rebuilds from its peers. A crash in the middle leaves `rebuilding` set,
/// and the next boot starts over.
async fn rebuild(
    ctx: &SimContext,
    ledger: &Ledger,
    durability: Durability,
) -> Result<Journal<SimStorageProvider>, Ended> {
    let (acked, floor, meta) = ledger.with(|state| {
        state.rebuilding = true;
        state.pending = None;
        state.target = None;
        (state.acked.clone(), state.floor, state.meta.clone())
    });
    let provider = ctx.storage().clone();
    let io = |error: std::io::Error| Ended::Open(OpenError::Io(error));
    if provider.exists(DIR).await.map_err(io)? {
        for name in provider.list_dir(DIR).await.map_err(io)? {
            provider
                .delete(&format!("{DIR}/{name}"))
                .await
                .map_err(io)?;
        }
    }
    let mut journal = Journal::create(provider, DIR, ID, config(durability), &meta)
        .await
        .map_err(Ended::Open)?;
    for (position, (id, payload)) in &acked {
        let mut batch = Batch::new();
        batch.put(*position, *id, payload.clone());
        journal.commit(batch).await.map_err(Ended::Commit)?;
    }
    // The floor and the metainfo last: the metainfo records the last batch.
    let mut batch = Batch::new();
    batch.truncate_prefix(floor).set_meta(meta);
    journal.commit(batch).await.map_err(Ended::Commit)?;
    assert_reachable!("the node rebuilt its journal from the ledger");
    ledger.with(|state| {
        state.rebuilding = false;
        state.last = LastBatch {
            witnessed: true,
            before: BTreeMap::new(),
        };
    });
    Ok(journal)
}

/// Before an open: a recycled spare the last boot left behind, which the
/// open keeps for its next roll.
async fn note_spares(ctx: &SimContext) {
    let Ok(names) = ctx.storage().list_dir(DIR).await else {
        return;
    };
    if names.iter().any(|n| n.starts_with("spare-")) {
        assert_reachable!("an open found a recycled spare segment");
    }
}

/// Record what this recovery exercised.
async fn note_recovery(ctx: &SimContext, recovery: &Recovery) {
    assert_sometimes!(recovery.torn > 0, "recovery discarded a torn record");
    if recovery.rebuilt > 0 {
        assert_reachable!("recovery rebuilt a persist record from its entry");
    }
    if !recovery.ambiguous.is_empty() {
        assert_reachable!("recovery found an ambiguous last batch");
    }
    if !recovery.corrupt.is_empty() {
        assert_reachable!("recovery found a corrupt entry in the last batch");
    }
    if recovery.meta_repaired {
        assert_reachable!("recovery repaired a metainfo copy");
    }
    let segments = ctx.storage().list_dir(DIR).await.map_or(0, |names| {
        names.iter().filter(|n| n.starts_with("seg-")).count()
    });
    assert_sometimes!(segments > 1, "the journal spans several segments");
}

/// What the oracle allows the replay to return.
struct Expected<'a> {
    acked: &'a BTreeMap<u64, Value>,
    pending: &'a Pending,
    /// What a commit that reported `Durable::No` tried to write.
    absent_puts: &'a [(u64, Value)],
    last: &'a LastBatch,
}

/// What the replay returned, and what the node repairs.
struct Judged {
    found: BTreeMap<u64, Value>,
    /// Damaged acknowledged entries, to write again.
    rewrite: Vec<(u64, Value)>,
    /// Damaged entries nothing acknowledged, to clear.
    clear: Vec<u64>,
}

impl Expected<'_> {
    /// Check each position the replay returned.
    fn judge(&self, replay: Vec<(u64, Result<moonpool_journal::Entry, ReadError>)>) -> Judged {
        let mut found: BTreeMap<u64, Value> = BTreeMap::new();
        let mut rewrite: Vec<(u64, Value)> = Vec::new();
        let mut clear = Vec::new();
        for (position, read) in replay {
            match read {
                Ok(entry) => {
                    let value = (entry.id, entry.payload);
                    assert_always!(
                        !self.absent_puts
                            .iter()
                            .any(|(p, v)| *p == position && *v == value),
                        "a commit that reported Durable::No is never live after a reopen",
                        { "position" => position }
                    );
                    let written = self.acked.get(&position) == Some(&value)
                        || self.pending.put_at(position).any(|v| *v == value)
                        || self.last.may_read_as(position, Some(&value));
                    assert_always!(
                        written,
                        "a position returns only its acknowledged value or one in flight",
                        { "position" => position }
                    );
                    found.insert(position, value);
                }
                Err(ReadError::Damaged { position: at, id }) => {
                    let named = self.acked.get(&position).is_some_and(|(i, _)| *i == id)
                        || self.pending.put_at(position).any(|(i, _)| *i == id)
                        || self.last.may_be_damaged_as(position, &id);
                    assert_always!(
                        at == position && named,
                        "damage names the position and an id written there",
                        { "position" => position }
                    );
                    match self.acked.get(&position) {
                        Some(value) => {
                            assert_reachable!("the node wrote a damaged acknowledged entry again");
                            rewrite.push((position, value.clone()));
                        }
                        None => clear.push(position),
                    }
                }
                Err(error) => {
                    assert_always!(false, "a replay reports damage, nothing else", {
                        "position" => position, "error" => error
                    });
                }
            }
        }
        Judged {
            found,
            rewrite,
            clear,
        }
    }
}

/// The reporting oracle: check the recovered journal against the ledger.
/// Then play the replication layer: write a damaged acknowledged entry
/// again, clear a damaged entry nothing acknowledged, and adopt what the
/// disk holds.
async fn reconcile(
    journal: &mut Journal<SimStorageProvider>,
    ledger: &Ledger,
) -> Result<(), CommitError> {
    let (acked, floor, meta, mut pending, last) = ledger.with(|state| {
        (
            state.acked.clone(),
            state.floor,
            state.meta.clone(),
            state.pending.clone().unwrap_or_default(),
            state.last.clone(),
        )
    });
    let absent = pending.known_absent;
    if absent {
        assert_always!(
            journal.floor() == floor && journal.meta() == meta.as_slice(),
            "a commit that reported Durable::No is never live after a reopen",
            { "floor" => journal.floor(), "acked" => floor }
        );
    }
    // What the failed commit tried to write, kept for the check below; the
    // other checks then treat the disk as holding no batch in flight.
    let absent_puts = if absent {
        std::mem::take(&mut pending).puts
    } else {
        Vec::new()
    };
    assert_always!(
        journal.floor() == floor || pending.floor.is_some_and(|f| journal.floor() == f.max(floor)),
        "the floor is the acknowledged one or the one in flight",
        { "floor" => journal.floor(), "acked" => floor }
    );
    assert_always!(
        journal.meta() == meta.as_slice() || pending.meta.as_deref() == Some(journal.meta()),
        "the metainfo is the acknowledged one or the one in flight"
    );

    let replay = match journal.replay(..).await {
        Ok(replay) => replay,
        Err(error) => {
            return Err(CommitError::Io {
                error,
                durable: Durable::No,
            });
        }
    };
    let expected = Expected {
        acked: &acked,
        pending: &pending,
        absent_puts: &absent_puts,
        last: &last,
    };
    let Judged {
        mut found,
        rewrite,
        clear,
    } = expected.judge(replay);
    for (position, value) in &acked {
        let kept = found.contains_key(position)
            || rewrite.iter().any(|(p, _)| p == position)
            || pending.touches(*position)
            || *position < journal.floor()
            || last.may_read_as(*position, found.get(position));
        assert_always!(
            kept,
            "an acknowledged entry is returned or reported damaged, never forgotten",
            { "position" => *position, "id" => format!("{:02x?}", &value.0[..16]) }
        );
    }
    if !rewrite.is_empty() || !clear.is_empty() {
        // The metainfo records the repair batch: it is witnessed.
        let mut batch = Batch::new();
        for (position, (id, payload)) in &rewrite {
            batch.put(*position, *id, payload.clone());
            found.insert(*position, (*id, payload.clone()));
        }
        for position in &clear {
            batch.clear(*position..*position + 1);
        }
        batch.set_meta(journal.meta().to_vec());
        journal.commit(batch).await?;
    }
    ledger.with(|state| {
        state.acked = found;
        state.floor = journal.floor();
        state.meta = journal.meta().to_vec();
        state.pending = None;
        state.target = None;
        // The open recorded the last batch it found, and a repair batch
        // carries the metainfo.
        state.last = LastBatch {
            witnessed: true,
            before: BTreeMap::new(),
        };
    });
    Ok(())
}

/// An id unique to `(position, version)`.
fn id_of(position: u64, version: u64) -> Id {
    let mut id = [0; ID_SIZE];
    id[..8].copy_from_slice(&position.to_le_bytes());
    id[8..16].copy_from_slice(&version.to_le_bytes());
    id
}

/// A batch of entries at distinct random positions, sometimes a tombstone, a
/// floor raise or new metainfo.
fn draw(ctx: &SimContext, floor: u64, version: u64) -> Pending {
    let random = ctx.random();
    let mut pending = Pending::default();
    for _ in 0..random.random_range(1..6) {
        let position = floor + random.random_range(0..WINDOW);
        if pending.puts.iter().any(|(p, _)| *p == position) {
            continue;
        }
        let len = random.random_range(0..2048);
        let payload: Vec<u8> = (0..len).map(|_| random.random::<u8>()).collect();
        pending
            .puts
            .push((position, (id_of(position, version), payload)));
    }
    match random.random_range(0..12) {
        0 => {
            let start = floor + random.random_range(0..WINDOW);
            pending
                .clears
                .push(start..start + random.random_range(1..8));
        }
        1 => pending.floor = Some(floor + random.random_range(1..12)),
        2 => pending.meta = Some(format!("term={version}").into_bytes()),
        _ => {}
    }
    if let Some(new_floor) = pending.floor {
        pending.puts.retain(|(p, _)| *p >= new_floor);
    }
    pending
}

/// One commit of a random batch.
async fn step(
    ctx: &SimContext,
    journal: &mut Journal<SimStorageProvider>,
    ledger: &Ledger,
) -> Result<(), CommitError> {
    let floor = journal.floor();
    let version = ledger.with(|state| {
        state.version += 1;
        state.version
    });
    let pending = draw(ctx, floor, version);
    let batch = pending.batch();
    ledger.with(|state| state.pending = Some(pending.clone()));
    let rolls = journal.rolls();
    if let Err(error) = journal.commit(batch).await {
        if matches!(
            error,
            CommitError::Io {
                durable: Durable::No,
                ..
            }
        ) {
            ledger.with(|state| {
                if let Some(pending) = state.pending.as_mut() {
                    pending.known_absent = true;
                }
            });
        }
        return Err(error);
    }
    if journal.rolls().onto_spare > rolls.onto_spare {
        assert_reachable!("a commit rolled onto a recycled segment");
    }
    let target = Target {
        puts: pending
            .puts
            .iter()
            .filter_map(|(position, _)| journal.layout(*position))
            .collect(),
        puts_only: pending.clears.is_empty(),
    };
    // Acknowledged: the batch is durable.
    ledger.with(|state| state.ack(pending, target));
    Ok(())
}

/// Keeps the run going while attrition crashes the node, then checks the
/// node made progress.
pub struct JournalWorkload;

#[async_trait]
impl Workload for JournalWorkload {
    fn name(&self) -> &'static str {
        "journal_client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.time()
            .sleep(RUN_FOR)
            .await
            .map_err(|e| invalid_state(format!("sleep failed: {e}")))
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let acked = ctx
            .state()
            .get::<Ledger>(LEDGER)
            .map_or(0, |ledger| ledger.acked_len());
        assert_sometimes!(acked > 0, "the node ends the run with acknowledged entries");
        Ok(())
    }
}

/// Aimed double faults on the last acknowledged batch, then a crash: the
/// two-fault cases the random families almost never reach.
///
/// Each strike rots one byte of a persist record and one byte of its entry,
/// or one byte of every record of a batch of puts and of its first entry.
/// The bytes change on the durable image, so the crash keeps them.
pub struct JournalStrike;

/// Rot the byte at `at` of `path` on `ip`'s disk.
fn rot(ctx: &FaultContext, ip: &str, path: &str, at: u64) -> SimulationResult<()> {
    ctx.corrupt_bytes(ip, path, at..at + 1)
}

#[async_trait]
impl FaultInjector for JournalStrike {
    fn name(&self) -> &'static str {
        "journal_strike"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let ledger = Ledger::shared(ctx.state());
        while !ctx.chaos_shutdown().is_cancelled() {
            let wait = Duration::from_millis(ctx.random().random_range(500..4000));
            let _ = ctx.time().sleep(wait).await;
            let Some(ip) = ctx.process_ips().into_iter().next() else {
                continue;
            };
            if ctx.is_dead(&ip) {
                continue;
            }
            let Some(target) = ledger.with(|state| state.target.take()) else {
                continue;
            };
            let Some(first) = target.puts.first() else {
                continue;
            };
            let whole = target.puts_only && ctx.random().random_bool(0.3);
            if whole {
                for layout in &target.puts {
                    rot(
                        ctx,
                        &ip,
                        &layout.record.path,
                        layout.record.bytes.start + 20,
                    )?;
                }
                rot(ctx, &ip, &first.entry.path, first.entry.bytes.end - 1)?;
                assert_reachable!("a strike rotted every record and the first entry of a batch");
            } else {
                let at = ctx.random().random_range(0..target.puts.len());
                let layout = &target.puts[at];
                rot(
                    ctx,
                    &ip,
                    &layout.record.path,
                    layout.record.bytes.start + 20,
                )?;
                rot(ctx, &ip, &layout.entry.path, layout.entry.bytes.end - 1)?;
                assert_reachable!("a strike rotted a record and its entry");
            }
            ctx.crash_for(&ip, Duration::from_millis(50))?;
        }
        Ok(())
    }
}
