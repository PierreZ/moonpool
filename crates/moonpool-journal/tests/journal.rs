//! The journal over the simulator's storage: round trips, rollover,
//! truncation, targeted corruption, and randomized crashes.

use std::future::Future;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_journal::{Entry, Geometry, Journal, JournalConfig, JournalError, Record, Recovery};
use moonpool_sim::{SimStorageProvider, SimWorld, StorageConfiguration};

const DIR: &str = "wal";

fn ip() -> IpAddr {
    "10.0.0.1".parse().expect("valid IP")
}

/// A small geometry: one-block slot table (128 slots), 16 KiB data start,
/// 128 KiB segments — rollover is cheap to reach.
fn small() -> JournalConfig {
    JournalConfig {
        geometry: Geometry {
            slot_count: 128,
            data_start: 16 * 1024,
            segment_size: 128 * 1024,
        },
        max_batch_bytes: 16 * 1024,
        ..JournalConfig::default()
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
}

fn sim(seed: u64) -> SimWorld {
    let mut sim = SimWorld::new_with_seed(seed);
    sim.set_storage_config(StorageConfiguration::fast_local());
    sim
}

/// Run `f` against `sim`'s storage, stepping the world until it finishes.
async fn run<F, Fut, T>(sim: &mut SimWorld, f: F) -> T
where
    F: FnOnce(SimStorageProvider) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handle = tokio::spawn(f(sim.storage_provider(ip())));
    while !handle.is_finished() {
        while sim.pending_event_count() > 0 {
            sim.step();
        }
        tokio::task::yield_now().await;
    }
    handle.await.expect("task panicked")
}

fn payload(index: u64, len: usize) -> Vec<u8> {
    let seed = index.wrapping_mul(31).to_le_bytes()[0];
    (0..len).map(|at| seed ^ at.to_le_bytes()[0]).collect()
}

async fn open(
    provider: SimStorageProvider,
    config: JournalConfig,
) -> Result<(Journal<SimStorageProvider>, Recovery), JournalError> {
    Journal::open(provider, DIR, config).await
}

/// Append one entry per call (one batch each), with payload sizes from `lens`.
async fn append_each(
    journal: &mut Journal<SimStorageProvider>,
    epoch: u64,
    lens: &[usize],
) -> Result<(), JournalError> {
    for len in lens {
        let index = journal.next_index();
        let bytes = payload(index, *len);
        journal
            .append(&[Record {
                epoch,
                payload: &bytes,
            }])
            .await?;
    }
    Ok(())
}

async fn read_all(journal: &Journal<SimStorageProvider>) -> Result<Vec<Entry>, JournalError> {
    let mut entries = Vec::new();
    for index in journal.start_index()..journal.next_index() {
        entries.push(journal.read(index).await?);
    }
    Ok(entries)
}

/// Flip one bit at `offset` of `path` — bit rot at an exact spot.
async fn flip(sim: &mut SimWorld, path: String, offset: u64) {
    run(sim, |provider| async move {
        let file = provider.open(&path, OpenOptions::read_write()).await?;
        let mut byte = [0u8; 1];
        file.read_at(offset, &mut byte).await?;
        byte[0] ^= 0x10;
        file.write_at(offset, &byte).await?;
        file.sync_all().await
    })
    .await
    .expect("flip a bit");
}

fn segment_path(first: u64) -> String {
    format!("{DIR}/seg-{first:020}.wal")
}

#[test]
fn entries_round_trip_across_reopen() {
    runtime().block_on(async {
        let mut sim = sim(1);
        let written = run(&mut sim, |provider| async move {
            let (mut journal, recovery) = open(provider, small()).await?;
            assert!(recovery.created);
            let bytes: Vec<Vec<u8>> = (1..=10)
                .map(|i| payload(i, 100 + usize::try_from(i).expect("small")))
                .collect();
            let records: Vec<Record<'_>> = bytes
                .iter()
                .map(|payload| Record { epoch: 3, payload })
                .collect();
            let range = journal.append(&records).await?;
            assert_eq!(range, 1..11);
            read_all(&journal).await
        })
        .await
        .expect("write");

        let (read, recovery) = run(&mut sim, |provider| async move {
            let (journal, recovery) = open(provider, small()).await?;
            Ok::<_, JournalError>((read_all(&journal).await?, recovery))
        })
        .await
        .expect("reopen");
        assert_eq!(read, written);
        assert_eq!(
            recovery,
            Recovery::default(),
            "a clean reopen repairs nothing"
        );
        assert_eq!(read[4].payload, payload(5, 105));
    });
}

#[test]
fn segments_roll_over_and_recover_in_order() {
    runtime().block_on(async {
        let mut sim = sim(2);
        let lens: Vec<usize> = (0..300).map(|i| 50 + (i * 37) % 900).collect();
        let lens_again = lens.clone();
        run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider.clone(), small()).await?;
            // Mix single-entry batches with multi-entry ones.
            append_each(&mut journal, 1, &lens[..150]).await?;
            for chunk in lens[150..].chunks(7) {
                let first = journal.next_index();
                let bytes: Vec<Vec<u8>> = chunk
                    .iter()
                    .enumerate()
                    .map(|(at, len)| payload(first + at as u64, *len))
                    .collect();
                let records: Vec<Record<'_>> = bytes
                    .iter()
                    .map(|payload| Record { epoch: 2, payload })
                    .collect();
                journal.append(&records).await?;
            }
            assert!(provider.exists(&segment_path(1)).await?);
            Ok::<_, JournalError>(())
        })
        .await
        .expect("write");

        run(&mut sim, |provider| async move {
            let (journal, recovery) = open(provider.clone(), small()).await?;
            assert_eq!(recovery, Recovery::default());
            assert_eq!(journal.next_index(), 301);
            for (at, entry) in read_all(&journal).await?.iter().enumerate() {
                let index = at as u64 + 1;
                assert_eq!(entry.index, index);
                assert_eq!(entry.payload, payload(index, lens_again[at]));
                assert_eq!(entry.epoch, if index <= 150 { 1 } else { 2 });
            }
            Ok::<_, JournalError>(())
        })
        .await
        .expect("reopen");
    });
}

#[test]
fn suffix_truncation_makes_room_for_a_new_epoch() {
    runtime().block_on(async {
        let mut sim = sim(3);
        run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider, small()).await?;
            append_each(&mut journal, 1, &[3000; 40]).await?; // several segments
            journal.truncate_suffix(12).await?;
            assert_eq!(journal.next_index(), 12);
            append_each(&mut journal, 2, &[20; 5]).await?;
            Ok::<_, JournalError>(())
        })
        .await
        .expect("write");

        run(&mut sim, |provider| async move {
            let (journal, recovery) = open(provider.clone(), small()).await?;
            assert_eq!(recovery, Recovery::default());
            assert_eq!(journal.next_index(), 17);
            let epochs: Vec<u64> = (1..17).map(|i| journal.epoch(i).expect("live")).collect();
            assert_eq!(epochs, [[1; 11].as_slice(), [2; 5].as_slice()].concat());
            assert_eq!(journal.read(14).await?.payload, payload(14, 20));
            assert!(!provider.exists(&segment_path(40)).await?);
            Ok::<_, JournalError>(())
        })
        .await
        .expect("reopen");
    });
}

#[test]
fn prefix_truncation_drops_whole_segments_and_persists_the_start() {
    runtime().block_on(async {
        let mut sim = sim(4);
        run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider.clone(), small()).await?;
            append_each(&mut journal, 1, &[3000; 100]).await?;
            journal.truncate_prefix(60).await?;
            assert!(!provider.exists(&segment_path(1)).await?);
            assert!(matches!(
                journal.read(59).await,
                Err(JournalError::OutOfRange { .. })
            ));
            Ok::<_, JournalError>(())
        })
        .await
        .expect("write");

        run(&mut sim, |provider| async move {
            let (journal, _) = open(provider, small()).await?;
            assert_eq!((journal.start_index(), journal.next_index()), (60, 101));
            assert_eq!(read_all(&journal).await?.len(), 41);
            Ok::<_, JournalError>(())
        })
        .await
        .expect("reopen");
    });
}

#[test]
fn metadata_keeps_the_newest_generation() {
    runtime().block_on(async {
        let mut sim = sim(5);
        run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider, small()).await?;
            assert_eq!(journal.meta(), None);
            journal.save_meta(b"term=1 vote=a").await?;
            journal.save_meta(b"term=2 vote=b").await?;
            journal.save_meta(b"term=3 vote=c").await?;
            Ok::<_, JournalError>(())
        })
        .await
        .expect("write");
        // Damage the newest copy (generation 3 lives in meta.1): the older
        // intact copy wins.
        flip(&mut sim, format!("{DIR}/meta.1"), 30).await;
        run(&mut sim, |provider| async move {
            let (journal, _) = open(provider, small()).await?;
            assert_eq!(journal.meta(), Some(b"term=2 vote=b".as_slice()));
            Ok::<_, JournalError>(())
        })
        .await
        .expect("reopen");
    });
}

/// Five single-entry batches: entry `i` sits at `data_start + (i-1) × 4 KiB`.
fn entry_offset(index: u64) -> u64 {
    16 * 1024 + (index - 1) * 4096
}

/// Slot `i` of the first segment.
fn slot_offset(index: u64) -> u64 {
    8192 + (index - 1) * 32
}

async fn five_entries(sim: &mut SimWorld) {
    run(sim, |provider| async move {
        let (mut journal, _) = open(provider, small()).await?;
        append_each(&mut journal, 7, &[64; 5]).await
    })
    .await
    .expect("write");
}

#[test]
fn a_damaged_entry_mid_log_is_reported_not_truncated() {
    runtime().block_on(async {
        let mut sim = sim(6);
        five_entries(&mut sim).await;
        // A payload byte: the entry's CRC fails, its slot is intact.
        flip(&mut sim, segment_path(1), entry_offset(3) + 40).await;
        run(&mut sim, |provider| async move {
            let (journal, recovery) = open(provider, small()).await?;
            assert_eq!(recovery.corrupt, vec![(3, 7)]);
            assert!(!recovery.torn_tail);
            assert_eq!(journal.next_index(), 6, "nothing after the damage is lost");
            assert!(matches!(
                journal.read(3).await,
                Err(JournalError::Corrupt { index: 3, epoch: 7 })
            ));
            assert_eq!(journal.read(4).await?.payload, payload(4, 64));
            Ok::<_, JournalError>(())
        })
        .await
        .expect("reopen");
    });
}

#[test]
fn damaged_slots_are_rebuilt_from_intact_entries() {
    runtime().block_on(async {
        let mut sim = sim(7);
        five_entries(&mut sim).await;
        for index in 1..=5 {
            flip(&mut sim, segment_path(1), slot_offset(index) + 9).await;
        }
        run(&mut sim, |provider| async move {
            let (journal, recovery) = open(provider, small()).await?;
            assert_eq!(recovery.slots_rewritten, 5);
            assert!(recovery.corrupt.is_empty());
            assert_eq!(read_all(&journal).await?.len(), 5);
            Ok::<_, JournalError>(())
        })
        .await
        .expect("reopen");
        // The rewrite made the table durable: the next open is clean.
        run(&mut sim, |provider| async move {
            let (_, recovery) = open(provider, small()).await?;
            assert_eq!(recovery, Recovery::default());
            Ok::<_, JournalError>(())
        })
        .await
        .expect("second reopen");
    });
}

#[test]
fn a_double_fault_mid_log_refuses_to_start() {
    runtime().block_on(async {
        let mut sim = sim(8);
        five_entries(&mut sim).await;
        // Entry 3 and its slot, and the slots after it too: finding 4 and 5
        // takes the resynchronisation, not a slot.
        for index in 3..=5 {
            flip(&mut sim, segment_path(1), slot_offset(index) + 9).await;
        }
        flip(&mut sim, segment_path(1), entry_offset(3) + 40).await;
        let opened = run(&mut sim, |provider| async move {
            open(provider, small()).await.map(|_| ())
        })
        .await;
        assert!(
            matches!(opened, Err(JournalError::DoubleFault { index: 3 })),
            "{opened:?}"
        );
    });
}

#[test]
fn a_damaged_segment_header_is_repaired_from_its_twin() {
    runtime().block_on(async {
        let mut sim = sim(9);
        five_entries(&mut sim).await;
        flip(&mut sim, segment_path(1), 8).await;
        run(&mut sim, |provider| async move {
            let (journal, recovery) = open(provider, small()).await?;
            assert_eq!(recovery.headers_repaired, 1);
            assert_eq!(journal.next_index(), 6);
            Ok::<_, JournalError>(())
        })
        .await
        .expect("reopen");
    });
}

/// A tiny deterministic generator for the crash test's own choices.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// Storage whose crashes do everything the barrier-bounded model allows —
/// lose, shear, rot, or roll back unsynced sectors — but which never damages
/// a synced sector. Every recovery must therefore keep every acknowledged
/// entry and must never report corruption.
///
/// The severity is drawn per seed, swarm-style: a brutal crash rarely leaves
/// a later entry of the torn batch intact, and a mild one rarely tears
/// anything, and the interesting holes live in between.
fn crashy(rng: &mut Rng) -> StorageConfiguration {
    let mut pick =
        |choices: &[f64]| choices[usize::try_from(rng.below(choices.len() as u64)).expect("small")];
    let mut config = StorageConfiguration::fast_local();
    config.clean_crash_probability = pick(&[0.0, 0.1]);
    config.crash_lost_probability = pick(&[0.02, 0.1, 0.3]);
    config.crash_latent_fault_probability = pick(&[0.0, 0.02, 0.1]);
    config.shorn_write_probability = pick(&[0.0, 0.05, 0.2]);
    config.correlated_rollback_probability = pick(&[0.0, 0.2]);
    config.garbage_fill_probability = pick(&[0.0, 0.5, 1.0]);
    config
}

/// What the writer knows.
#[derive(Default)]
struct Ledger {
    /// `(index, epoch, payload)` for every acknowledged entry, in order.
    acked: Vec<(u64, u64, Vec<u8>)>,
    /// First index of every acknowledged `append` call: where a suffix cut
    /// rewrites no block holding kept entries.
    batch_starts: Vec<u64>,
}

#[test]
fn crashes_never_lose_an_acknowledged_entry_or_look_like_corruption() {
    let seeds: u64 = std::env::var("JOURNAL_CRASH_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(250);
    let mut torn = 0;
    let mut ambiguous = 0;
    let only: Option<u64> = std::env::var("JOURNAL_CRASH_SEED")
        .ok()
        .and_then(|s| s.parse().ok());
    for seed in only.map_or(1..=seeds, |seed| seed..=seed) {
        let (t, a) = crash_loop(seed);
        torn += t;
        ambiguous += a;
    }
    // The crashes must actually land mid-write, or this proves nothing.
    assert!(torn > 0, "no crash ever tore a tail");
    eprintln!("{seeds} seeds: {torn} torn tails, {ambiguous} ambiguous entries");
}

type Shared = Arc<Mutex<Ledger>>;

/// Crash a writer repeatedly; returns how many recoveries found a torn tail
/// and how many ambiguous entries they reported.
fn crash_loop(seed: u64) -> (usize, usize) {
    runtime().block_on(async {
        let mut torn = 0;
        let mut ambiguous = 0;
        let mut sim = SimWorld::new_with_seed(seed);
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        sim.set_storage_config(crashy(&mut rng));
        let ledger: Shared = Arc::new(Mutex::new(Ledger::default()));

        for round in 0..6 {
            let recovery =
                recover_and_check(&mut sim, &ledger, &format!("seed {seed} round {round}")).await;
            torn += usize::from(recovery.torn_tail);
            ambiguous += recovery.ambiguous_tail.len();

            // Write for a while, then crash mid-flight.
            let ops = 1 + rng.below(12);
            let plan: Vec<(u64, u64, u64)> = (0..ops)
                .map(|_| {
                    let len = if rng.below(2) == 0 {
                        16 + rng.below(200)
                    } else {
                        16 + rng.below(3000)
                    };
                    (rng.below(10), 1 + rng.below(9), len)
                })
                .collect();
            let handle = tokio::spawn(write(
                sim.storage_provider(ip()),
                round + 1,
                plan,
                Arc::clone(&ledger),
            ));
            let budget = rng.below(400);
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
        }
        (torn, ambiguous)
    })
}

/// Reopen the journal and check every acknowledged entry survived intact and
/// nothing was reported corrupt. What survived beyond the acknowledged
/// prefix becomes part of the log the writer builds on.
async fn recover_and_check(sim: &mut SimWorld, ledger: &Shared, at: &str) -> Recovery {
    let expected = ledger.lock().expect("ledger").acked.clone();
    let recovered = run(sim, |provider| async move {
        let (journal, recovery) = open(provider, small()).await?;
        Ok::<_, JournalError>((recovery, read_all(&journal).await?))
    })
    .await;
    let (recovery, entries) =
        recovered.unwrap_or_else(|error| panic!("{at}: open failed: {error}"));
    assert!(
        recovery.corrupt.is_empty(),
        "{at}: a crash was reported as corruption: {recovery:?}"
    );
    assert!(
        entries.len() >= expected.len(),
        "{at}: lost acknowledged entries ({} < {})",
        entries.len(),
        expected.len()
    );
    for ((index, epoch, payload), entry) in expected.iter().zip(&entries) {
        assert_eq!(
            (entry.index, entry.epoch, &entry.payload),
            (*index, *epoch, payload),
            "{at}: acknowledged entry changed"
        );
    }
    let mut ledger = ledger.lock().expect("ledger");
    let next = entries.last().map_or(0, |entry| entry.index + 1);
    ledger.batch_starts.retain(|start| *start < next);
    ledger.acked = entries
        .into_iter()
        .map(|entry| (entry.index, entry.epoch, entry.payload))
        .collect();
    recovery
}

/// The writer: `(kind, count, len)` steps of appends and, one time in ten,
/// a suffix truncation at an acknowledged batch start.
async fn write(
    provider: SimStorageProvider,
    epoch: u64,
    plan: Vec<(u64, u64, u64)>,
    ledger: Shared,
) -> Result<(), JournalError> {
    let (mut journal, _) = open(provider, small()).await?;
    for (kind, count, len) in plan {
        let len = usize::try_from(len).expect("small");
        let cut = {
            let starts = &ledger.lock().expect("ledger").batch_starts;
            (kind == 0 && !starts.is_empty()).then(|| starts[len % starts.len()])
        };
        if let Some(from) = cut {
            // Raft-style: drop a suffix, then rewrite it. The dropped entries
            // stop being guaranteed the moment the truncation starts.
            {
                let mut ledger = ledger.lock().expect("ledger");
                ledger.acked.retain(|(index, _, _)| *index < from);
                ledger.batch_starts.retain(|start| *start < from);
            }
            journal.truncate_suffix(from).await?;
            continue;
        }
        let next = journal.next_index();
        let bytes: Vec<Vec<u8>> = (0..count).map(|at| payload(next + at, len)).collect();
        let records: Vec<Record<'_>> = bytes
            .iter()
            .map(|payload| Record { epoch, payload })
            .collect();
        let range = journal.append(&records).await?;
        let mut ledger = ledger.lock().expect("ledger");
        ledger.batch_starts.push(range.start);
        for (index, payload) in range.zip(bytes) {
            ledger.acked.push((index, epoch, payload));
        }
    }
    Ok(())
}
