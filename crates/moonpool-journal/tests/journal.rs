//! The journal on the simulator's storage: positions in any order, reopen,
//! rollover, the floor and tombstones, every row of the recovery table, the
//! metainfo copies, and crash loops under two fault models and both
//! durability modes.

use std::future::Future;
use std::net::IpAddr;

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_journal::{
    Batch, CommitError, Durability, Geometry, ID_SIZE, Id, Journal, JournalConfig, JournalId,
    Layout, OpenError, ReadError, Recovery, State,
};
use moonpool_sim::{SimStorageProvider, SimWorld, StorageConfiguration};

mod support;
use support::{CrashModel, crash_loop};

pub(crate) const DIR: &str = "wal";
pub(crate) const JOURNAL: JournalId = JournalId(0x00C0_FFEE);

pub(crate) fn ip() -> IpAddr {
    "10.0.0.1".parse().expect("valid IP")
}

pub(crate) fn config(durability: Durability) -> JournalConfig {
    JournalConfig {
        durability,
        geometry: Geometry::small(),
        ..JournalConfig::default()
    }
}

pub(crate) fn runtime() -> tokio::runtime::Runtime {
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
pub(crate) async fn run<F, Fut, T>(sim: &mut SimWorld, f: F) -> T
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

/// An id every byte of which depends on `position` and `version`.
pub(crate) fn id(position: u64, version: u64) -> Id {
    let mut id = [0; ID_SIZE];
    id[..8].copy_from_slice(&position.to_le_bytes());
    id[8..16].copy_from_slice(&version.to_le_bytes());
    id[16..].copy_from_slice(&(position ^ version.rotate_left(17)).to_le_bytes());
    id
}

pub(crate) fn payload(position: u64, version: u64, len: usize) -> Vec<u8> {
    let seed = position.wrapping_mul(31) ^ version.wrapping_mul(131);
    (0..len)
        .map(|at| (seed.wrapping_add(at as u64) % 251) as u8)
        .collect()
}

type J = Journal<SimStorageProvider>;

async fn create(sim: &mut SimWorld, durability: Durability) -> J {
    run(sim, move |provider| async move {
        Journal::create(provider, DIR, JOURNAL, config(durability), b"born").await
    })
    .await
    .expect("create")
}

async fn reopen(sim: &mut SimWorld, durability: Durability) -> Result<(J, Recovery), OpenError> {
    run(sim, move |provider| async move {
        Journal::open(provider, DIR, JOURNAL, config(durability)).await
    })
    .await
    .map(|found| found.expect("a journal is there"))
}

/// Commit one batch of puts `(position, version, len)`.
async fn put(sim: &mut SimWorld, journal: J, puts: &[(u64, u64, usize)]) -> J {
    let mut batch = Batch::new();
    for &(position, version, len) in puts {
        batch.put(
            position,
            id(position, version),
            payload(position, version, len),
        );
    }
    commit(sim, journal, batch).await.expect("commit")
}

async fn commit(sim: &mut SimWorld, mut journal: J, batch: Batch) -> Result<J, CommitError> {
    run(sim, move |_| async move {
        journal.commit(batch).await.map(|()| journal)
    })
    .await
}

async fn read(sim: &mut SimWorld, journal: J, position: u64) -> (J, Result<Vec<u8>, ReadError>) {
    run(sim, move |_| async move {
        let read = journal.read(position).await.map(|entry| entry.payload);
        (journal, read)
    })
    .await
}

/// Flip one bit of `path` at `offset`: bit rot at an exact spot.
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

/// Zero `len` bytes of `path` at `offset`: a lost write.
async fn zero(sim: &mut SimWorld, path: String, offset: u64, len: u64) {
    run(sim, move |provider| async move {
        let file = provider.open(&path, OpenOptions::read_write()).await?;
        file.write_at(offset, &vec![0; usize::try_from(len).expect("small")])
            .await?;
        file.sync_all().await
    })
    .await
    .expect("zero a range");
}

/// The persist record and entry of `position`, as `(path, range)` pairs.
fn spots(journal: &J, position: u64) -> ((String, u64), (String, u64)) {
    let layout = journal.layout(position).expect("a live position");
    assert_eq!(layout.record.stripe, Some(position));
    (
        (layout.record.path, layout.record.bytes.start + 20),
        (layout.entry.path, layout.entry.bytes.end - 1),
    )
}

const BOTH: [Durability; 2] = [Durability::Ordered, Durability::Batched];

#[test]
fn positions_in_any_order_survive_reopen() {
    runtime().block_on(async {
        for durability in BOTH {
            let mut sim = sim(1);
            let journal = create(&mut sim, durability).await;
            // A lagging writer: high positions first, gaps, then old ones,
            // then an overwrite.
            let journal = put(&mut sim, journal, &[(40, 1, 100), (3, 1, 10)]).await;
            let journal = put(&mut sim, journal, &[(7, 1, 0), (1, 1, 3000)]).await;
            let journal = put(&mut sim, journal, &[(3, 2, 55)]).await;
            drop(journal);
            let (journal, recovery) = reopen(&mut sim, durability).await.expect("reopen");
            assert_eq!(recovery, Recovery::default(), "{durability:?}");
            let positions: Vec<u64> = journal.positions().map(|(p, _)| p).collect();
            assert_eq!(positions, vec![1, 3, 7, 40]);
            assert_eq!(journal.meta(), b"born");
            let mut journal = journal;
            for (position, version, len) in [(1, 1, 3000), (3, 2, 55), (7, 1, 0), (40, 1, 100)] {
                let (back, bytes) = read(&mut sim, journal, position).await;
                journal = back;
                assert_eq!(bytes.expect("intact"), payload(position, version, len));
                assert_eq!(
                    journal.state(position),
                    State::Live {
                        id: id(position, version),
                        len: u32::try_from(len).expect("small"),
                    }
                );
            }
        }
    });
}

#[test]
fn an_empty_directory_holds_no_journal_and_create_refuses_an_existing_one() {
    runtime().block_on(async {
        let mut sim = sim(2);
        let none = run(&mut sim, |provider| async move {
            Journal::open(provider, DIR, JOURNAL, config(Durability::Ordered))
                .await
                .map(|found| found.is_none())
        })
        .await;
        assert!(none.expect("open"), "no journal yet");
        drop(create(&mut sim, Durability::Ordered).await);
        let again = run(&mut sim, |provider| async move {
            Journal::create(provider, DIR, JOURNAL, config(Durability::Ordered), b"")
                .await
                .map(drop)
        })
        .await;
        assert!(matches!(again, Err(OpenError::AlreadyExists)), "{again:?}");
        let wrong = run(&mut sim, |provider| async move {
            Journal::open(provider, DIR, JournalId(7), config(Durability::Ordered))
                .await
                .map(drop)
        })
        .await;
        assert!(
            matches!(wrong, Err(OpenError::WrongJournal { .. })),
            "{wrong:?}"
        );
    });
}

#[test]
fn the_floor_and_tombstones_free_whole_segments_without_resurrection() {
    runtime().block_on(async {
        let mut sim = sim(3);
        let mut journal = create(&mut sim, Durability::Ordered).await;
        // Each batch takes a fresh persist block: eight per segment.
        for position in 0..30 {
            journal = put(&mut sim, journal, &[(position, 1, 2000)]).await;
        }
        // Overwrite an old position late: its new copy lives in the newest
        // segment, the old one in the first.
        journal = put(&mut sim, journal, &[(2, 2, 10)]).await;
        let mut batch = Batch::new();
        batch
            .clear(0..2)
            .clear(3..20)
            .truncate_prefix(1)
            .set_meta("moved");
        journal = commit(&mut sim, journal, batch).await.expect("commit");
        let live: Vec<u64> = journal.positions().map(|(p, _)| p).collect();
        assert_eq!(live, [vec![2], (20..30).collect()].concat());
        drop(journal);
        let (journal, _) = reopen(&mut sim, Durability::Ordered).await.expect("reopen");
        let live: Vec<u64> = journal.positions().map(|(p, _)| p).collect();
        assert_eq!(
            live,
            [vec![2], (20..30).collect()].concat(),
            "nothing resurrected"
        );
        assert_eq!(journal.floor(), 1);
        assert_eq!(journal.meta(), b"moved");
        let (_, bytes) = read(&mut sim, journal, 2).await;
        assert_eq!(bytes.expect("the overwrite"), payload(2, 2, 10));
        let names = run(
            &mut sim,
            |provider| async move { provider.list_dir(DIR).await },
        )
        .await
        .expect("list");
        let segments = names.iter().filter(|n| n.starts_with("seg-")).count();
        assert!(
            segments < 4,
            "dead leading segments were deleted: {names:?}"
        );
        let regions = journal_regions(&mut sim).await;
        assert_eq!(regions.iter().filter(|r| r.kind == Layout::META).count(), 2);
        assert_eq!(
            regions.iter().filter(|r| r.kind == Layout::HEADER).count(),
            2 * segments
        );
        let striped: Vec<u64> = regions
            .iter()
            .filter(|r| r.kind == Layout::ENTRY)
            .filter_map(|r| r.stripe)
            .collect();
        assert_eq!(striped, [vec![2], (20..30).collect()].concat());
    });
}

#[test]
fn a_put_below_the_floor_is_refused_before_anything_is_written() {
    runtime().block_on(async {
        let mut sim = sim(4);
        let journal = create(&mut sim, Durability::Ordered).await;
        let mut batch = Batch::new();
        batch.truncate_prefix(10).put(9, id(9, 1), vec![1]);
        let refused = commit(&mut sim, journal, batch).await;
        assert!(
            matches!(
                refused,
                Err(CommitError::BelowFloor {
                    position: 9,
                    floor: 10
                })
            ),
            "{refused:?}"
        );
    });
}

async fn journal_regions(sim: &mut SimWorld) -> Vec<moonpool_core::LayoutRegion> {
    let (journal, _) = reopen(sim, Durability::Ordered).await.expect("reopen");
    journal.regions()
}

/// Five batches of one entry each at positions 1..=5.
async fn five(sim: &mut SimWorld, durability: Durability) -> J {
    let mut journal = create(sim, durability).await;
    for position in 1..=5 {
        journal = put(sim, journal, &[(position, 1, 300)]).await;
    }
    journal
}

#[test]
fn a_damaged_entry_before_the_last_batch_is_reported_by_identity_never_truncated() {
    runtime().block_on(async {
        for durability in BOTH {
            let mut sim = sim(5);
            let journal = five(&mut sim, durability).await;
            let (_, entry) = spots(&journal, 2);
            drop(journal);
            flip(&mut sim, entry.0, entry.1).await;
            let (journal, recovery) = reopen(&mut sim, durability).await.expect("opens");
            assert_eq!(recovery, Recovery::default(), "checked when read, not at open");
            let (journal, bytes) = read(&mut sim, journal, 2).await;
            assert!(
                matches!(bytes, Err(ReadError::Damaged { position: 2, id: got }) if got == id(2, 1)),
                "{bytes:?}"
            );
            let replay = run(&mut sim, move |_| async move { journal.replay(..).await })
                .await
                .expect("replay");
            let damaged: Vec<u64> = replay
                .iter()
                .filter(|(_, read)| read.is_err())
                .map(|(p, _)| *p)
                .collect();
            assert_eq!(damaged, vec![2]);
            assert_eq!(replay.len(), 5, "the replay went on past it");
        }
    });
}

#[test]
fn a_damaged_last_entry_is_corrupt_when_ordered_and_ambiguous_when_batched() {
    runtime().block_on(async {
        for (durability, ambiguous) in [(Durability::Ordered, false), (Durability::Batched, true)] {
            let mut sim = sim(6);
            let journal = five(&mut sim, durability).await;
            let (_, entry) = spots(&journal, 5);
            drop(journal);
            flip(&mut sim, entry.0, entry.1).await;
            let (journal, recovery) = reopen(&mut sim, durability).await.expect("opens");
            let expected = vec![(5, id(5, 1))];
            if ambiguous {
                assert_eq!(recovery.ambiguous, expected);
                assert_eq!(journal.state(5), State::Ambiguous { id: id(5, 1) });
                // Another batch commits: the damaged entry is no longer the
                // last, so its persist record proves it was synced.
                let journal = put(&mut sim, journal, &[(6, 1, 10)]).await;
                assert_eq!(journal.state(5), State::Corrupt { id: id(5, 1) });
            } else {
                assert_eq!(recovery.corrupt, expected);
                assert_eq!(journal.state(5), State::Corrupt { id: id(5, 1) });
            }
        }
    });
}

#[test]
fn a_damaged_record_is_rebuilt_from_its_intact_entry() {
    runtime().block_on(async {
        for durability in BOTH {
            for position in [2, 5] {
                let mut sim = sim(7);
                let journal = five(&mut sim, durability).await;
                let (record, _) = spots(&journal, position);
                drop(journal);
                flip(&mut sim, record.0, record.1).await;
                let (journal, recovery) = reopen(&mut sim, durability).await.expect("opens");
                assert_eq!(recovery.rebuilt, 1, "{durability:?} at {position}");
                let (_, bytes) = read(&mut sim, journal, position).await;
                assert_eq!(bytes.expect("intact"), payload(position, 1, 300));
            }
        }
    });
}

#[test]
fn a_damaged_record_and_entry_before_the_last_batch_refuse_to_open() {
    runtime().block_on(async {
        let mut sim = sim(8);
        let journal = five(&mut sim, Durability::Ordered).await;
        let journal = put(&mut sim, journal, &[(10, 1, 300), (11, 1, 300)]).await;
        let journal = put(&mut sim, journal, &[(12, 1, 300)]).await;
        let (record, entry) = spots(&journal, 11);
        drop(journal);
        flip(&mut sim, record.0, record.1).await;
        flip(&mut sim, entry.0, entry.1).await;
        let refused = reopen(&mut sim, Durability::Ordered).await.map(drop);
        assert!(
            matches!(refused, Err(OpenError::DoubleFault { index: 1, .. })),
            "{refused:?}"
        );
    });
}

/// Every record of a batch and its first entry damaged, before the last
/// batch: nothing even says how many records it held.
#[test]
fn a_batch_with_no_surviving_identity_before_the_last_is_lost() {
    runtime().block_on(async {
        let mut sim = sim(8);
        let journal = five(&mut sim, Durability::Ordered).await;
        let (record, entry) = spots(&journal, 3);
        drop(journal);
        flip(&mut sim, record.0, record.1).await;
        flip(&mut sim, entry.0, entry.1).await;
        let refused = reopen(&mut sim, Durability::Ordered).await.map(drop);
        assert!(
            matches!(refused, Err(OpenError::LostBatch { .. })),
            "{refused:?}"
        );
    });
}

#[test]
fn a_torn_last_record_is_discarded_durably() {
    runtime().block_on(async {
        for durability in BOTH {
            let mut sim = sim(9);
            let journal = five(&mut sim, durability).await;
            let journal = put(&mut sim, journal, &[(6, 1, 300), (7, 1, 300)]).await;
            let (record, entry) = spots(&journal, 7);
            drop(journal);
            flip(&mut sim, record.0, record.1).await;
            flip(&mut sim, entry.0, entry.1).await;
            let (journal, recovery) = reopen(&mut sim, durability).await.expect("opens");
            assert_eq!(recovery.torn, 1, "{durability:?}");
            assert_eq!(journal.state(7), State::Empty);
            assert!(matches!(journal.state(6), State::Live { .. }));
            // More batches, then reopen: the discarded record is not damage
            // before the last batch now; opening wrote its verdict down.
            let journal = put(&mut sim, journal, &[(8, 1, 10)]).await;
            drop(journal);
            let (journal, recovery) = reopen(&mut sim, durability).await.expect("still opens");
            assert_eq!(recovery, Recovery::default());
            let live: Vec<u64> = journal.positions().map(|(p, _)| p).collect();
            assert_eq!(live, vec![1, 2, 3, 4, 5, 6, 8]);
        }
    });
}

#[test]
fn a_lost_batch_in_the_middle_is_refused_not_read_as_the_end() {
    runtime().block_on(async {
        let mut sim = sim(10);
        let journal = five(&mut sim, Durability::Ordered).await;
        let (record, entry) = spots(&journal, 3);
        drop(journal);
        // Batch 3's persist block and its entry's block, lost.
        zero(&mut sim, record.0, record.1 / 4096 * 4096, 4096).await;
        zero(&mut sim, entry.0, entry.1 / 4096 * 4096, 4096).await;
        let refused = reopen(&mut sim, Durability::Ordered).await.map(drop);
        assert!(
            matches!(refused, Err(OpenError::LostBatch { .. })),
            "{refused:?}"
        );
    });
}

#[test]
fn a_tail_the_metainfo_proves_durable_is_never_lost_silently() {
    runtime().block_on(async {
        let mut sim = sim(11);
        let journal = five(&mut sim, Durability::Ordered).await;
        // The metainfo records batch 5 as durable.
        let mut batch = Batch::new();
        batch.set_meta("after five");
        let journal = commit(&mut sim, journal, batch).await.expect("meta");
        let (record, entry) = spots(&journal, 5);
        drop(journal);
        zero(&mut sim, record.0, record.1 / 4096 * 4096, 4096).await;
        zero(&mut sim, entry.0, entry.1 / 4096 * 4096, 4096).await;
        let refused = reopen(&mut sim, Durability::Ordered).await.map(drop);
        assert!(
            matches!(refused, Err(OpenError::LostBatch { .. })),
            "{refused:?}"
        );
    });
}

#[test]
fn a_missing_segment_is_refused() {
    runtime().block_on(async {
        let mut sim = sim(12);
        let mut journal = create(&mut sim, Durability::Ordered).await;
        for position in 0..20 {
            journal = put(&mut sim, journal, &[(position, 1, 100)]).await;
        }
        drop(journal);
        let names = run(
            &mut sim,
            |provider| async move { provider.list_dir(DIR).await },
        )
        .await
        .expect("list");
        let segments: Vec<&String> = names.iter().filter(|n| n.starts_with("seg-")).collect();
        assert!(segments.len() >= 3, "{names:?}");
        let middle = format!("{DIR}/{}", segments[1]);
        run(&mut sim, move |provider| async move {
            provider.delete(&middle).await
        })
        .await
        .expect("delete");
        let refused = reopen(&mut sim, Durability::Ordered).await.map(drop);
        assert!(
            matches!(refused, Err(OpenError::LostBatch { .. })),
            "{refused:?}"
        );
    });
}

#[test]
fn metainfo_survives_either_copy_and_refuses_losing_both() {
    runtime().block_on(async {
        for (damage, outcome) in [
            (vec![0u64], true),
            (vec![16 * 4096], true),
            (vec![0, 16 * 4096], false),
        ] {
            let mut sim = sim(13);
            let journal = create(&mut sim, Durability::Ordered).await;
            let mut batch = Batch::new();
            batch.set_meta("promise 9");
            drop(commit(&mut sim, journal, batch).await.expect("meta"));
            for at in &damage {
                flip(&mut sim, format!("{DIR}/meta"), at + 30).await;
            }
            match reopen(&mut sim, Durability::Ordered).await {
                Ok((journal, recovery)) => {
                    assert!(outcome, "{damage:?}");
                    assert!(recovery.meta_repaired);
                    assert_eq!(journal.meta(), b"promise 9");
                }
                Err(error) => {
                    assert!(!outcome, "{damage:?}: {error}");
                    assert!(matches!(error, OpenError::MetaLost));
                }
            }
        }
    });
}

#[test]
fn peeking_reads_the_metainfo_and_changes_nothing() {
    runtime().block_on(async {
        let mut sim = sim(16);
        let none = run(&mut sim, |provider| async move {
            Journal::peek_meta(&provider, DIR, JOURNAL).await
        })
        .await
        .expect("peek");
        assert_eq!(none, None, "no journal yet");
        let journal = create(&mut sim, Durability::Ordered).await;
        let mut batch = Batch::new();
        batch.set_meta("formatted");
        drop(commit(&mut sim, journal, batch).await.expect("meta"));
        // Damage copy A: a peek still reads B, and repairs nothing.
        flip(&mut sim, format!("{DIR}/meta"), 30).await;
        let peeked = run(&mut sim, |provider| async move {
            Journal::peek_meta(&provider, DIR, JOURNAL).await
        })
        .await
        .expect("peek");
        assert_eq!(peeked.as_deref(), Some(&b"formatted"[..]));
        let (_, recovery) = reopen(&mut sim, Durability::Ordered).await.expect("reopen");
        assert!(
            recovery.meta_repaired,
            "the peek left the repair to the open"
        );
    });
}

#[test]
fn segments_without_metainfo_are_a_lost_journal_not_an_empty_one() {
    runtime().block_on(async {
        let mut sim = sim(14);
        drop(five(&mut sim, Durability::Ordered).await);
        run(&mut sim, |provider| async move {
            provider.delete(&format!("{DIR}/meta")).await
        })
        .await
        .expect("delete");
        let refused = reopen(&mut sim, Durability::Ordered).await.map(drop);
        assert!(matches!(refused, Err(OpenError::MetaLost)), "{refused:?}");
    });
}

fn seeds(default: u64) -> std::ops::RangeInclusive<u64> {
    let env = |name: &str| std::env::var(name).ok().and_then(|s| s.parse().ok());
    if let Some(seed) = env("JOURNAL_CRASH_SEED") {
        return seed..=seed;
    }
    1..=env("JOURNAL_CRASH_SEEDS").unwrap_or(default)
}

/// The crash loop under the paper's fault model, for one durability setting
/// (`None` draws it afresh every round). One test per setting, so each stays
/// within its time budget and they run in parallel.
fn paper(durability: Option<Durability>) {
    let mut tally = support::Tally::default();
    for seed in seeds(120) {
        crash_loop(seed, CrashModel::Paper, durability, &mut tally);
    }
    eprintln!("paper {durability:?}: {tally:?}");
    assert!(tally.recoveries > 0 && tally.commits > 0);
    assert!(
        tally.torn + tally.rebuilt > 0,
        "no crash ever landed mid-batch"
    );
}

/// The crash loop under moonpool's full crash physics, for one durability
/// setting (`None` draws it afresh every round).
fn harsh(durability: Option<Durability>) {
    let mut tally = support::Tally::default();
    for seed in seeds(120) {
        crash_loop(seed, CrashModel::Harsh, durability, &mut tally);
    }
    eprintln!("harsh {durability:?}: {tally:?}");
    assert!(tally.torn > 0, "no crash ever tore a record");
    assert!(tally.prefix_dropped > 0, "no segment was ever freed");
}

#[test]
fn ordered_under_the_papers_fault_model_nothing_acknowledged_is_lost_or_damaged() {
    paper(Some(Durability::Ordered));
}

#[test]
fn batched_under_the_papers_fault_model_nothing_acknowledged_is_lost_or_damaged() {
    paper(Some(Durability::Batched));
}

#[test]
fn mixed_under_the_papers_fault_model_nothing_acknowledged_is_lost_or_damaged() {
    paper(None);
}

#[test]
fn ordered_under_moonpools_full_physics_nothing_acknowledged_is_lost_or_damaged() {
    harsh(Some(Durability::Ordered));
}

#[test]
fn batched_under_moonpools_full_physics_nothing_acknowledged_is_lost_or_damaged() {
    harsh(Some(Durability::Batched));
}

#[test]
fn mixed_under_moonpools_full_physics_nothing_acknowledged_is_lost_or_damaged() {
    harsh(None);
}

#[test]
fn acknowledged_commits_survive_a_crash() {
    runtime().block_on(async {
        for durability in BOTH {
            let mut sim = sim(15);
            let journal = create(&mut sim, durability).await;
            let journal = put(&mut sim, journal, &[(2, 1, 100)]).await;
            let mut batch = Batch::new();
            batch.truncate_prefix(1).set_meta("m2");
            let journal = commit(&mut sim, journal, batch).await.expect("commit");
            drop(journal);
            sim.simulate_crash_for_process(ip(), true);
            let (journal, _) = reopen(&mut sim, durability).await.expect("reopen");
            assert_eq!(journal.floor(), 1, "{durability:?}");
            assert_eq!(journal.meta(), b"m2");
            assert!(
                matches!(journal.state(2), State::Live { .. }),
                "{durability:?}"
            );
        }
    });
}
