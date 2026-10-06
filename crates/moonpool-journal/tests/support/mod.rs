//! The crash loop: a writer commits random batches (positions in any order,
//! overwrites, tombstones, floor raises, metainfo), is cut off mid-commit by
//! a crash or a bare restart, and every reopen is judged against a ledger of
//! what was acknowledged and what was in flight.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use moonpool_journal::{Batch, Durability, Id, Journal, OpenError, ReadError};
use moonpool_sim::{SimStorageProvider, SimWorld, StorageConfiguration};

use super::{DIR, JOURNAL, config, id, ip, payload, run, runtime};

#[derive(Clone, Copy, Debug)]
pub(crate) enum CrashModel {
    /// The paper's: every unsynced sector ends up old or new, plus clean
    /// crashes and correlated rollbacks.
    Paper,
    /// Moonpool's full physics on top: sectors lost to garbage, latent read
    /// faults, shorn writes. These destroy what an in-flight sector held,
    /// even bytes a sync made durable; the journal never rewrites such a
    /// sector, so the promise is the same.
    Harsh,
}

#[derive(Debug, Default)]
pub(crate) struct Tally {
    pub recoveries: usize,
    pub commits: usize,
    pub restarts: usize,
    pub open_retries: usize,
    pub torn: u32,
    pub rebuilt: u32,
    pub ambiguous: usize,
    pub meta_repaired: usize,
    pub segments_removed: u32,
    /// Seeds whose first segment no longer starts at batch 1.
    pub prefix_dropped: usize,
    pub crashed_opens: usize,
}

/// A tiny deterministic generator.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n.max(1)
    }
    fn pick<T: Copy>(&mut self, options: &[T]) -> T {
        options[usize::try_from(self.below(options.len() as u64)).expect("small")]
    }
}

fn storage(model: CrashModel, rng: &mut Rng) -> StorageConfiguration {
    let mut config = StorageConfiguration::fast_local();
    config.clean_crash_probability = rng.pick(&[0.0, 0.1]);
    config.correlated_rollback_probability = rng.pick(&[0.0, 0.2]);
    config.garbage_fill_probability = rng.pick(&[0.0, 0.5, 1.0]);
    config.crash_lost_probability = 0.0;
    config.crash_latent_fault_probability = 0.0;
    config.shorn_write_probability = 0.0;
    config.sync_failure_probability = rng.pick(&[0.0, 0.02, 0.05]);
    config.unsynced_dir_entry_loss_probability = rng.pick(&[0.0, 0.5, 1.0]);
    if matches!(model, CrashModel::Harsh) {
        config.crash_lost_probability = rng.pick(&[0.02, 0.1, 0.3]);
        config.crash_latent_fault_probability = rng.pick(&[0.0, 0.02, 0.1]);
        config.shorn_write_probability = rng.pick(&[0.0, 0.05, 0.2]);
    }
    config
}

/// A value at a position: its id and payload.
type Value = (Id, Vec<u8>);

/// What the writer knows.
#[derive(Debug, Default, Clone)]
struct Ledger {
    acked: BTreeMap<u64, Value>,
    floor: u64,
    meta: Vec<u8>,
    /// The batch in flight when the writer stopped.
    pending: Option<Pending>,
    commits: usize,
}

#[derive(Debug, Default, Clone)]
struct Pending {
    /// Whether the batch was committed with `Durability::Batched`.
    batched: bool,
    puts: Vec<(u64, Value)>,
    clears: Vec<Range<u64>>,
    floor: Option<u64>,
    meta: Option<Vec<u8>>,
}

impl Ledger {
    /// Apply an acknowledged batch.
    fn ack(&mut self, pending: Pending) {
        for (position, value) in pending.puts {
            self.acked.insert(position, value);
        }
        for range in pending.clears {
            self.acked.retain(|p, _| !range.contains(p));
        }
        if let Some(floor) = pending.floor {
            self.floor = self.floor.max(floor);
        }
        if let Some(meta) = pending.meta {
            self.meta = meta;
        }
        let floor = self.floor;
        self.acked.retain(|p, _| *p >= floor);
        self.commits += 1;
    }
}

type Shared = Arc<Mutex<Ledger>>;

type J = Journal<SimStorageProvider>;

/// Crash a writer over and over, judging each recovery.
/// `durability: None` draws the mode afresh every round: it is recorded per
/// batch, so a journal may change it between opens.
pub(crate) fn crash_loop(
    seed: u64,
    model: CrashModel,
    durability: Option<Durability>,
    tally: &mut Tally,
) {
    runtime().block_on(async {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut sim = SimWorld::new_with_seed(seed);
        sim.set_storage_config(storage(model, &mut rng));
        let ledger: Shared = Arc::new(Mutex::new(Ledger {
            meta: b"born".to_vec(),
            ..Ledger::default()
        }));
        let mut version = 0u64;
        for round in 0..8 {
            let durability = durability.unwrap_or_else(|| {
                if rng.below(2) == 0 {
                    Durability::Ordered
                } else {
                    Durability::Batched
                }
            });
            let at = format!("{model:?} {durability:?} seed {seed} round {round}");
            // Sometimes the crash lands inside recovery itself: while it
            // voids torn records, repairs a metainfo copy, deletes segments.
            if round > 0 && rng.below(4) == 0 {
                let budget = rng.below(60);
                let handle = tokio::spawn(Journal::open(
                    sim.storage_provider(ip()),
                    DIR,
                    JOURNAL,
                    config(durability),
                ));
                for _ in 0..budget {
                    if handle.is_finished() {
                        break;
                    }
                    if sim.pending_event_count() > 0 {
                        sim.step();
                    }
                    tokio::task::yield_now().await;
                }
                handle.abort();
                let _ = handle.await;
                sim.simulate_crash_for_process(ip(), true);
                tally.crashed_opens += 1;
            }
            let journal = recover(&mut sim, &ledger, durability, &at, tally).await;
            let ops = 1 + rng.below(10);
            let mut plan = Vec::new();
            for _ in 0..ops {
                version += 1;
                plan.push(random_batch(&mut rng, &ledger, version));
            }
            for pending in &mut plan {
                pending.batched = durability == Durability::Batched;
            }
            let handle = tokio::spawn(write(journal, plan, Arc::clone(&ledger)));
            let budget = rng.below(500);
            for _ in 0..budget {
                if handle.is_finished() {
                    break;
                }
                if sim.pending_event_count() > 0 {
                    sim.step();
                }
                tokio::task::yield_now().await;
            }
            handle.abort();
            let _ = handle.await;
            let restart = rng.below(4) == 0;
            if std::env::var("JOURNAL_DEBUG").is_ok() {
                eprintln!("--- stopped after {budget} steps, restart only: {restart}");
            }
            if restart {
                // The process alone dies: the page cache survives.
                tally.restarts += 1;
            } else {
                sim.simulate_crash_for_process(ip(), true);
            }
        }
        tally.commits += ledger.lock().expect("ledger").commits;
        let names = run(&mut sim, |provider| async move {
            moonpool_core::StorageProvider::list_dir(&provider, DIR).await
        })
        .await
        .unwrap_or_default();
        // The first batch a journal ever writes is generation 1's first.
        let first = names.iter().find(|name| name.starts_with("seg-"));
        if first.is_some_and(|name| name != &format!("seg-{:020}.wal", (1u64 << 40) + 1)) {
            tally.prefix_dropped += 1;
        }
    });
}

fn random_batch(rng: &mut Rng, ledger: &Shared, version: u64) -> Pending {
    let floor = ledger.lock().expect("ledger").floor;
    let mut pending = Pending::default();
    for _ in 0..=rng.below(5) {
        // Positions in a window above the floor, in any order.
        let position = floor + rng.below(48);
        let len = usize::try_from(if rng.below(2) == 0 {
            rng.below(200)
        } else {
            rng.below(3000)
        })
        .expect("small");
        pending.puts.push((
            position,
            (id(position, version), payload(position, version, len)),
        ));
    }
    match rng.below(10) {
        0 => {
            let start = floor + rng.below(48);
            pending.clears.push(start..start + 1 + rng.below(4));
        }
        1 => pending.floor = Some(floor + rng.below(8)),
        2 => pending.meta = Some(format!("meta v{version}").into_bytes()),
        _ => {}
    }
    if let Some(new_floor) = pending.floor {
        pending.puts.retain(|(p, _)| *p >= new_floor);
    }
    pending
}

async fn write(mut journal: J, plan: Vec<Pending>, ledger: Shared) {
    for pending in plan {
        let mut batch = Batch::new();
        for (position, (id, bytes)) in &pending.puts {
            batch.put(*position, *id, bytes.clone());
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
        ledger.lock().expect("ledger").pending = Some(pending.clone());
        let outcome = journal.commit(batch).await;
        if std::env::var("JOURNAL_DEBUG").is_ok() {
            eprintln!(
                "commit puts {:?} clears {:?} floor {:?} -> {:?} (journal {journal:?})",
                pending.puts.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
                pending.clears,
                pending.floor,
                outcome.as_ref().err()
            );
        }
        if outcome.is_err() {
            return;
        }
        let mut ledger = ledger.lock().expect("ledger");
        ledger.pending = None;
        ledger.ack(pending);
    }
}

/// Open (creating on the first round), judge what opening found against
/// the ledger, then play the replication layer: discard the damaged
/// entries of the batch that was in flight.
async fn recover(
    sim: &mut SimWorld,
    ledger: &Shared,
    durability: Durability,
    at: &str,
    tally: &mut Tally,
) -> J {
    for _ in 0..64 {
        let opened = run(sim, move |provider| async move {
            match Journal::open(provider.clone(), DIR, JOURNAL, config(durability)).await? {
                Some(found) => Ok(found),
                None => Journal::create(provider, DIR, JOURNAL, config(durability), b"born")
                    .await
                    .map(|journal| (journal, moonpool_journal::Recovery::default())),
            }
        })
        .await;
        let (journal, recovery) = match opened {
            Ok(found) => found,
            Err(OpenError::Io(_)) => {
                tally.open_retries += 1;
                continue;
            }
            Err(error) => panic!("{at}: a crash must never stop the journal opening: {error}"),
        };
        tally.recoveries += 1;
        if std::env::var("JOURNAL_DEBUG").is_ok() {
            eprintln!("{at}: opened {journal:?} {recovery:?}");
        }
        tally.torn += recovery.torn;
        tally.rebuilt += recovery.rebuilt;
        tally.ambiguous += recovery.ambiguous.len();
        tally.meta_repaired += usize::from(recovery.meta_repaired);
        tally.segments_removed += recovery.segments_removed;
        match judge(sim, journal, ledger, at).await {
            Some(journal) => return journal,
            None => {
                tally.open_retries += 1;
            }
        }
    }
    panic!("{at}: the journal never opened");
}

/// Check every position, then make the ledger what the disk holds. `None`
/// when the discard of damaged entries failed (reopen and judge again).
async fn judge(sim: &mut SimWorld, journal: J, ledger: &Shared, at: &str) -> Option<J> {
    let snapshot = ledger.lock().expect("ledger").clone();
    let pending = snapshot.pending.clone().unwrap_or_default();
    let (journal, replay) = run(sim, move |_| async move {
        let replay = journal.replay(..).await;
        (journal, replay)
    })
    .await;
    let replay = replay.unwrap_or_else(|error| panic!("{at}: replay: {error}"));
    let found: BTreeMap<u64, Result<Value, ReadError>> = replay
        .into_iter()
        .map(|(p, read)| (p, read.map(|entry| (entry.id, entry.payload))))
        .collect();

    // The floor and the metainfo: acknowledged, or the in-flight batch's.
    let floor_ok = journal.floor() == snapshot.floor
        || pending
            .floor
            .is_some_and(|f| journal.floor() == f.max(snapshot.floor));
    assert!(
        floor_ok,
        "{at}: floor {} vs ledger {snapshot:?}",
        journal.floor()
    );
    let meta_ok = journal.meta() == snapshot.meta.as_slice()
        || pending.meta.as_deref() == Some(journal.meta());
    assert!(
        meta_ok,
        "{at}: metainfo {:?}",
        String::from_utf8_lossy(journal.meta())
    );

    let mut positions: Vec<u64> = snapshot.acked.keys().copied().collect();
    positions.extend(found.keys().copied());
    positions.extend(pending.puts.iter().map(|(p, _)| *p));
    positions.sort_unstable();
    positions.dedup();
    let mut damaged = Vec::new();
    for position in positions {
        let touched = pending.puts.iter().any(|(p, _)| *p == position)
            || pending.clears.iter().any(|r| r.contains(&position))
            || pending.floor.is_some_and(|f| position < f);
        let acked = snapshot.acked.get(&position);
        match found.get(&position) {
            None => assert!(
                acked.is_none() || touched || position < journal.floor(),
                "{at}: acknowledged position {position} is gone"
            ),
            Some(Ok(value)) => {
                let allowed = acked == Some(value)
                    || pending
                        .puts
                        .iter()
                        .any(|(p, v)| *p == position && v == value);
                assert!(
                    allowed,
                    "{at}: position {position} holds a value never written there"
                );
            }
            Some(Err(error)) => {
                assert!(
                    touched && pending.batched,
                    "{at}: position {position} damaged: {error} (acked {:?}, touched {touched})",
                    acked.map(|(id, _)| id)
                );
                damaged.push(position);
            }
        }
    }

    // The replication layer discards what it cannot trust; the ledger
    // becomes what the disk holds.
    let mut journal = journal;
    if !damaged.is_empty() {
        let mut batch = Batch::new();
        for position in &damaged {
            batch.clear(*position..*position + 1);
        }
        let outcome = run(sim, move |_| async move {
            journal.commit(batch).await.map(|()| journal)
        })
        .await;
        journal = outcome.ok()?;
    }
    let mut ledger = ledger.lock().expect("ledger");
    ledger.acked = found
        .into_iter()
        .filter_map(|(p, read)| read.ok().map(|value| (p, value)))
        .collect();
    ledger.floor = journal.floor();
    ledger.meta = journal.meta().to_vec();
    ledger.pending = None;
    Some(journal)
}
