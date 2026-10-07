//! Crash-aware journal example: `moonpool-journal` under crash attrition.
//!
//! A single [`JournalNode`] owns a CLSTORE journal on its simulated disk. On
//! every boot it opens the journal (running the recovery scan), checks what
//! survived against the ledger of acknowledged writes, and then commits
//! batches until attrition crashes it again: entries at positions in any
//! order (a lagging replica's fill-in), overwrites, tombstones, floor raises
//! and metainfo. Each boot draws its [`Durability`], so one run proves both
//! protocols and a journal that changes protocol between opens.
//!
//! The disk runs `Chaos::Storage(Random)` with the families a lone node
//! cannot repair masked: a crash resolves every unsynced sector old or new,
//! lost, latent or shorn, unsynced directory entries may vanish, syncs fail,
//! transfers come up short. The journal never rewrites a sector holding
//! anything acknowledged, so the node holds it to the paper's promise:
//!
//! - an acknowledged write is never lost or changed, and never reported
//!   damaged: damage there would be a crash mistaken for corruption;
//! - only the batch in flight at the crash may come back partly, and only a
//!   `Batched` one may come back with ambiguous entries, which the node
//!   discards as a replication layer would, since nothing acknowledged them.
//!
//! A failed sync is an operating error, not a verdict: the batch it covered
//! was never acknowledged, and the node reopens the journal.
//!
//! The ledger lives in the iteration's [`StateHandle`](moonpool_sim::StateHandle),
//! so it survives the reboots that the node's memory does not.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_journal::{
    Batch, CommitError, Durability, Geometry, ID_SIZE, Id, Journal, JournalConfig, JournalId,
    OpenError, Recovery,
};
use moonpool_sim::{
    Process, RandomProvider, SimContext, SimStorageProvider, SimulationResult, StorageProvider,
    TimeProvider, Workload, assert_always, assert_reachable, assert_sometimes,
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
    batched: bool,
    puts: Vec<(u64, Value)>,
    clears: Vec<Range<u64>>,
    floor: Option<u64>,
    meta: Option<Vec<u8>>,
}

impl Pending {
    fn touches(&self, position: u64) -> bool {
        self.puts.iter().any(|(p, _)| *p == position)
            || self.clears.iter().any(|r| r.contains(&position))
            || self.floor.is_some_and(|f| position < f)
    }
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
    /// Commits so far: each one's version makes its values unique.
    version: u64,
}

impl Ledger {
    /// The iteration's ledger, published on first use.
    fn shared(ctx: &SimContext) -> Self {
        if let Some(ledger) = ctx.state().get::<Self>(LEDGER) {
            return ledger;
        }
        let ledger = Self::default();
        ctx.state().publish(LEDGER, ledger.clone());
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
        let ledger = Ledger::shared(ctx);
        loop {
            let Some(life) = unless_shutdown(ctx, live(ctx, &ledger)).await else {
                return Ok(());
            };
            match life {
                // A failed sync (or another I/O error) poisons the journal:
                // reopen it, as its caller should.
                Ended::Open(OpenError::Io(_)) | Ended::Commit(CommitError::Io { .. }) => {
                    assert_reachable!("an I/O error made the node reopen its journal");
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

/// One opening of the journal: recover and check it, then write until an
/// error ends it.
async fn live(ctx: &SimContext, ledger: &Ledger) -> Ended {
    let durability = if ctx.random().random_bool(0.5) {
        Durability::Ordered
    } else {
        Durability::Batched
    };
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
        Err(error) => {
            assert_always!(false, "the journal recovers from every crash", {
                "error" => error
            });
            return Ended::Open(error);
        }
    };
    note_recovery(ctx, &recovery).await;
    if let Err(error) = reconcile(&mut journal, ledger).await {
        return Ended::Commit(error);
    }
    loop {
        if let Err(error) = step(ctx, &mut journal, ledger, durability).await {
            return Ended::Commit(error);
        }
        let pause = Duration::from_millis(ctx.random().random_range(1..20));
        // A sleep only fails at shutdown, which the caller watches for.
        let _ = ctx.time().sleep(pause).await;
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
    if recovery.meta_repaired {
        assert_reachable!("recovery repaired a metainfo copy");
    }
    let segments = ctx.storage().list_dir(DIR).await.map_or(0, |names| {
        names.iter().filter(|n| n.starts_with("seg-")).count()
    });
    assert_sometimes!(segments > 1, "the journal spans several segments");
}

/// Check the recovered journal against the ledger, discard what only the
/// batch in flight could have damaged, then adopt what the disk holds.
async fn reconcile(
    journal: &mut Journal<SimStorageProvider>,
    ledger: &Ledger,
) -> Result<(), CommitError> {
    let (acked, floor, meta, pending) = ledger.with(|state| {
        (
            state.acked.clone(),
            state.floor,
            state.meta.clone(),
            state.pending.clone().unwrap_or_default(),
        )
    });
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
                durable: moonpool_journal::Durable::No,
            });
        }
    };
    let mut found: BTreeMap<u64, Value> = BTreeMap::new();
    let mut damaged = Vec::new();
    for (position, read) in replay {
        match read {
            Ok(entry) => {
                let value = (entry.id, entry.payload);
                let written = acked.get(&position) == Some(&value)
                    || pending
                        .puts
                        .iter()
                        .any(|(p, v)| *p == position && *v == value);
                assert_always!(written, "a position holds only a value written there", {
                    "position" => position
                });
                found.insert(position, value);
            }
            Err(error) => {
                assert_always!(
                    pending.batched && pending.touches(position),
                    "only the Batched batch in flight comes back damaged",
                    { "position" => position, "error" => error }
                );
                damaged.push(position);
            }
        }
    }
    for position in acked.keys() {
        assert_always!(
            found.contains_key(position)
                || pending.touches(*position)
                || *position < journal.floor(),
            "no acknowledged write is lost",
            { "position" => *position }
        );
    }
    if !damaged.is_empty() {
        assert_reachable!("the node discarded the ambiguous entries of a crashed batch");
        let mut batch = Batch::new();
        for position in &damaged {
            batch.clear(*position..*position + 1);
        }
        journal.commit(batch).await?;
    }
    ledger.with(|state| {
        state.acked = found;
        state.floor = journal.floor();
        state.meta = journal.meta().to_vec();
        state.pending = None;
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

/// One commit: entries at random positions, sometimes a tombstone, a floor
/// raise or new metainfo.
async fn step(
    ctx: &SimContext,
    journal: &mut Journal<SimStorageProvider>,
    ledger: &Ledger,
    durability: Durability,
) -> Result<(), CommitError> {
    let random = ctx.random();
    let floor = journal.floor();
    let version = ledger.with(|state| {
        state.version += 1;
        state.version
    });
    let mut pending = Pending {
        batched: durability == Durability::Batched,
        ..Pending::default()
    };
    for _ in 0..random.random_range(1..6) {
        let position = floor + random.random_range(0..WINDOW);
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
    let mut batch = Batch::new();
    for (position, (id, payload)) in &pending.puts {
        batch.put(*position, *id, payload.clone());
    }
    for range in &pending.clears {
        batch.clear(range.clone());
    }
    if let Some(floor) = pending.floor {
        batch.truncate_prefix(floor);
    }
    if let Some(meta) = &pending.meta {
        batch.set_meta(meta.clone());
    }
    ledger.with(|state| state.pending = Some(pending.clone()));
    journal.commit(batch).await?;
    // Acknowledged: the batch is durable.
    ledger.with(|state| {
        for (position, value) in pending.puts {
            state.acked.insert(position, value);
        }
        for range in pending.clears {
            state.acked.retain(|p, _| !range.contains(p));
        }
        if let Some(floor) = pending.floor {
            state.floor = state.floor.max(floor);
            assert_reachable!("the node raised its floor");
        }
        if let Some(meta) = pending.meta {
            state.meta = meta;
        }
        let floor = state.floor;
        state.acked.retain(|p, _| *p >= floor);
        state.pending = None;
    });
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
