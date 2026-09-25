//! Crash-aware journal example: `moonpool-journal` under crash attrition.
//!
//! A single [`JournalNode`] owns a write-ahead journal on its simulated disk.
//! On every boot it opens the journal — running the CLSTORE recovery scan —
//! checks what survived against the ledger of acknowledged entries, and then
//! appends batches, truncates suffixes and prefixes, and saves metadata until
//! attrition crashes it again. The crash resolves every unsynced sector the
//! way moonpool's default disk does: old or new, reordered, sometimes rolled
//! back in correlated runs. That is the fault model the CLSTORE paper
//! assumes, so the node holds the journal to the paper's promise:
//!
//! - an acknowledged entry is never lost, never changed, and never reported
//!   corrupt — a checksum failure there would be a crash mistaken for
//!   corruption;
//! - only entries that were never acknowledged can be torn, and the journal
//!   either truncates them (no identifier, or the ambiguous last entry) or
//!   reports them corrupt, in which case the node plays the replication layer
//!   and discards them, since nothing acknowledged them.
//!
//! The ledger lives in the iteration's [`StateHandle`](moonpool_sim::StateHandle),
//! so it survives the reboots that the node's memory does not.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_journal::{Entry, Geometry, Journal, JournalConfig, JournalError, Record, Recovery};
use moonpool_sim::{
    Process, RandomProvider, SimContext, SimStorageProvider, SimulationResult, StorageProvider,
    TimeProvider, Workload, assert_always, assert_reachable, assert_sometimes,
};

use crate::support::{invalid_state, unless_shutdown};

/// The journal's directory on the node's disk.
const DIR: &str = "wal";

/// The [`StateHandle`](moonpool_sim::StateHandle) key the ledger lives under.
const LEDGER: &str = "journal_ledger";

/// How long the workload keeps the run going; attrition stops at the
/// builder's `chaos_duration`, and the rest is a quiet tail.
pub const RUN_FOR: Duration = Duration::from_secs(40);

/// A small geometry — 256 slots, 256 KiB segments — so a run rolls over
/// several segments and truncation crosses segment boundaries.
fn config() -> JournalConfig {
    JournalConfig {
        geometry: Geometry {
            slot_count: 256,
            data_start: 16 * 1024,
            segment_size: 256 * 1024,
        },
        ..JournalConfig::default()
    }
}

/// What the node has acknowledged, shared across its reboots.
#[derive(Clone, Default)]
pub struct Ledger(Arc<Mutex<LedgerState>>);

#[derive(Default)]
struct LedgerState {
    /// `(index, epoch, payload)` of every acknowledged entry still in the
    /// log, in order.
    acked: Vec<(u64, u64, Vec<u8>)>,
    /// Boots so far: each one writes in a fresh epoch.
    boots: u64,
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

    /// How many entries are acknowledged and still in the log.
    #[must_use]
    pub fn acked_len(&self) -> usize {
        self.with(|state| state.acked.len())
    }
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
        let epoch = ledger.with(|state| {
            state.boots += 1;
            state.boots
        });

        let open = Journal::open(ctx.storage().clone(), DIR, config());
        let Some(opened) = unless_shutdown(ctx, open).await else {
            return Ok(());
        };
        let (mut journal, recovery) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                assert_always!(false, "the journal recovers from every crash", {
                    "error" => error
                });
                return Err(invalid_state(format!("journal refused to open: {error}")));
            }
        };
        note_recovery(ctx, &recovery).await;

        let Some(checked) = unless_shutdown(ctx, reconcile(&mut journal, &ledger, &recovery)).await
        else {
            return Ok(());
        };
        checked.map_err(|error| invalid_state(format!("reconciling the journal: {error}")))?;

        loop {
            let Some(step) = unless_shutdown(ctx, step(ctx, &mut journal, &ledger, epoch)).await
            else {
                return Ok(());
            };
            if let Err(error) = step {
                assert_always!(false, "journal writes succeed on a healthy disk", {
                    "error" => error
                });
                return Err(invalid_state(format!("journal write failed: {error}")));
            }
            let pause = Duration::from_millis(ctx.random().random_range(1..20));
            if unless_shutdown(ctx, ctx.time().sleep(pause))
                .await
                .is_none()
            {
                return Ok(());
            }
        }
    }
}

/// Record what this recovery exercised.
async fn note_recovery(ctx: &SimContext, recovery: &Recovery) {
    assert_sometimes!(recovery.torn_tail, "recovery discarded a torn tail");
    if recovery.ambiguous_tail.is_some() {
        assert_reachable!("recovery truncated an ambiguous last entry");
    }
    if !recovery.corrupt.is_empty() {
        assert_reachable!("recovery reported an entry corrupt");
    }
    if recovery.slots_rewritten > 0 {
        assert_reachable!("recovery rebuilt a lost slot");
    }
    let segments = ctx.storage().list_dir(DIR).await.map_or(0, |names| {
        names.iter().filter(|n| n.starts_with("seg-")).count()
    });
    assert_sometimes!(segments > 1, "the journal spans several segments");
}

/// Check the recovered log against the ledger, then adopt it: every
/// acknowledged entry must be there, unchanged; what survived beyond them is
/// kept up to the first entry that cannot be read, which was never
/// acknowledged and so is discarded, as a replication layer would.
async fn reconcile(
    journal: &mut Journal<SimStorageProvider>,
    ledger: &Ledger,
    recovery: &Recovery,
) -> Result<(), JournalError> {
    let acked = ledger.with(|state| state.acked.clone());
    let acked_end = acked.last().map_or(0, |(index, _, _)| index + 1);

    assert_always!(
        recovery.corrupt.iter().all(|(index, _)| *index >= acked_end),
        "only unacknowledged entries are reported corrupt",
        { "corrupt" => format!("{:?}", recovery.corrupt), "acked_end" => acked_end }
    );
    assert_always!(
        journal.next_index() >= acked_end,
        "no acknowledged entry is lost",
        { "next" => journal.next_index(), "acked_end" => acked_end }
    );

    let mut kept: Vec<Entry> = Vec::new();
    for index in journal.start_index()..journal.next_index() {
        match journal.read(index).await {
            Ok(entry) => kept.push(entry),
            Err(JournalError::Corrupt { .. }) if index >= acked_end => {
                journal.truncate_suffix(index).await?;
                break;
            }
            Err(error) => return Err(error),
        }
    }
    let intact = acked.iter().all(|(index, epoch, payload)| {
        kept.iter().any(|entry| {
            entry.index == *index && entry.epoch == *epoch && entry.payload == *payload
        })
    });
    assert_always!(intact, "an acknowledged entry reads back unchanged");

    ledger.with(|state| {
        state.acked = kept
            .into_iter()
            .map(|entry| (entry.index, entry.epoch, entry.payload))
            .collect();
    });
    Ok(())
}

/// One step of the node's life: mostly an appended batch, sometimes a
/// suffix truncation, a compaction, or a metadata save.
async fn step(
    ctx: &SimContext,
    journal: &mut Journal<SimStorageProvider>,
    ledger: &Ledger,
    epoch: u64,
) -> Result<(), JournalError> {
    let random = ctx.random();
    let (start, next) = (journal.start_index(), journal.next_index());
    match random.random_range(0..20) {
        0 if next > start + 1 => {
            // Raft-style: drop a conflicting suffix. The dropped entries stop
            // being guaranteed the moment the truncation starts.
            let from = random.random_range(start + 1..next);
            ledger.with(|state| state.acked.retain(|(index, _, _)| *index < from));
            journal.truncate_suffix(from).await?;
            assert_reachable!("the node truncated a suffix");
        }
        1 if next > start => {
            // Compaction: whole segments before a point go.
            let before = random.random_range(start..next);
            journal.truncate_prefix(before).await?;
            let start = journal.start_index();
            ledger.with(|state| state.acked.retain(|(index, _, _)| *index >= start));
        }
        2 => {
            let meta = format!("term={epoch} vote=node");
            journal.save_meta(meta.as_bytes()).await?;
            assert_always!(
                journal.meta() == Some(meta.as_bytes()),
                "saved metadata reads back"
            );
        }
        _ => {
            let count = random.random_range(1..8);
            let payloads: Vec<Vec<u8>> = (0..count)
                .map(|_| {
                    let len = random.random_range(1..2048);
                    (0..len).map(|_| random.random::<u8>()).collect()
                })
                .collect();
            let records: Vec<Record<'_>> = payloads
                .iter()
                .map(|payload| Record { epoch, payload })
                .collect();
            let indexes = journal.append(&records).await?;
            // Acknowledged: the batch is durable.
            ledger.with(|state| {
                for (index, payload) in indexes.zip(payloads) {
                    state.acked.push((index, epoch, payload));
                }
            });
        }
    }
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
