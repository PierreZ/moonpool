//! The journal on the simulator's storage: positions in any order, reopen,
//! rollover, the floor and tombstones, every row of the recovery table, the
//! metainfo copies, and crash loops under two fault models and both
//! durability modes.

use std::future::Future;
use std::net::IpAddr;

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_journal::{
    Batch, CommitError, Durability, Geometry, ID_SIZE, Id, Journal, JournalConfig, JournalId,
    Layout, OpenError, ReadError, RecordRepair, Recovery, State,
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

async fn names(sim: &mut SimWorld) -> Vec<String> {
    run(sim, |provider| async move { provider.list_dir(DIR).await })
        .await
        .expect("list")
}

/// Commit a floor raise to `floor` with one put at `position`.
async fn raise_floor(sim: &mut SimWorld, journal: J, floor: u64, position: u64) -> J {
    let mut batch = Batch::new();
    batch
        .truncate_prefix(floor)
        .put(position, id(position, 1), payload(position, 1, 100));
    commit(sim, journal, batch).await.expect("commit")
}

fn count(names: &[String], prefix: &str) -> usize {
    names.iter().filter(|n| n.starts_with(prefix)).count()
}

#[test]
fn a_dead_segment_is_recycled_and_its_old_batches_read_as_stale() {
    runtime().block_on(async {
        for durability in BOTH {
            let mut sim = sim(16);
            let mut journal = create(&mut sim, durability).await;
            // Eight batches fill a segment's persist log: 0..8 fill the
            // first, 8..16 the second.
            for position in 0..16 {
                journal = put(&mut sim, journal, &[(position, 1, 100)]).await;
            }
            assert_eq!(journal.rolls().created, 2, "{durability:?}");
            // The floor frees the first segment: it becomes the spare.
            journal = raise_floor(&mut sim, journal, 8, 16).await;
            let listed = names(&mut sim).await;
            assert_eq!(count(&listed, "spare-"), 1, "{listed:?}");
            assert_eq!(count(&listed, "seg-"), 2, "{listed:?}");

            // A crash keeps the spare: it is a whole segment of old batches.
            drop(journal);
            sim.simulate_crash_for_process(ip(), true);
            let (mut journal, recovery) = reopen(&mut sim, durability).await.expect("reopen");
            assert!(recovery.spare_kept, "{recovery:?}");
            assert_eq!(recovery.spares_removed, 0);

            // The third segment fills; the next roll adopts the spare, whose
            // persist and entry logs still hold batches 1..=8.
            for position in 17..25 {
                journal = put(&mut sim, journal, &[(position, 1, 100)]).await;
            }
            assert_eq!(journal.rolls().onto_spare, 1, "{durability:?}");
            assert_eq!(journal.rolls().created, 0, "{durability:?}");
            let listed = names(&mut sim).await;
            assert_eq!(count(&listed, "spare-"), 0, "{listed:?}");

            // Reopened, the recycled segment holds its one new batch and
            // nothing of its old ones: no lost batch, no damage, no torn
            // record.
            drop(journal);
            sim.simulate_crash_for_process(ip(), true);
            let (mut journal, recovery) = reopen(&mut sim, durability).await.expect("reopen");
            assert!(recovery.corrupt.is_empty(), "{recovery:?}");
            assert!(recovery.ambiguous.is_empty(), "{recovery:?}");
            assert_eq!(recovery.torn, 0, "{recovery:?}");
            assert_eq!(recovery.rebuilt, 0, "{recovery:?}");
            assert_eq!(journal.positions().count(), 17, "{durability:?}");
            for position in 8..25 {
                let (back, bytes) = read(&mut sim, journal, position).await;
                journal = back;
                assert_eq!(bytes.expect("intact"), payload(position, 1, 100));
            }
            assert!(matches!(journal.state(0), State::Empty));
        }
    });
}

#[test]
fn a_spare_that_is_not_a_whole_segment_is_deleted_and_never_read() {
    runtime().block_on(async {
        let mut sim = sim(17);
        let mut journal = create(&mut sim, Durability::Ordered).await;
        journal = put(&mut sim, journal, &[(0, 1, 100)]).await;
        drop(journal);
        // A spare cut short, its bytes anything at all.
        run(&mut sim, |provider| async move {
            let path = format!("{DIR}/spare-0000000099-0000000000.wal");
            let file = provider
                .open(&path, OpenOptions::create_new_write())
                .await?;
            file.write_at(0, &[0xA5; 3 * 4096]).await?;
            file.sync_all().await
        })
        .await
        .expect("a stray spare");
        sim.simulate_crash_for_process(ip(), true);
        let (journal, recovery) = reopen(&mut sim, Durability::Ordered).await.expect("reopen");
        assert_eq!(recovery.spares_removed, 1);
        assert!(!recovery.spare_kept);
        assert!(matches!(journal.state(0), State::Live { .. }));
        let names = names(&mut sim).await;
        assert_eq!(count(&names, "spare-"), 0, "{names:?}");
        assert_eq!(count(&names, "seg-"), 1, "{names:?}");
    });
}

/// A crash in a roll onto the spare, after the rename and before the
/// directory sync, can keep both names: the spare's old one and the
/// segment's new one, one file behind both. The next open must not keep that
/// name as a spare, or the next roll writes another segment's header and
/// batches into a live segment.
#[test]
fn a_crash_mid_adopt_never_leaves_a_spare_that_is_a_live_segment() {
    runtime().block_on(async {
        for seed in 0..24 {
            a_crash_mid_adopt(seed).await;
        }
    });
}

/// Kills the caller at the roll's rename, and nowhere else.
struct KillAtRename;

impl moonpool_buggify::hint::Sink for KillAtRename {
    fn strike(&self, label: &'static str) -> moonpool_buggify::hint::Strike {
        if label == "journal roll: spare renamed, directory not synced" {
            moonpool_buggify::hint::Strike::Killed
        } else {
            moonpool_buggify::hint::Strike::None
        }
    }
}

async fn a_crash_mid_adopt(seed: u64) {
    let at = format!("seed {seed}");
    let mut sim = sim(seed);
    let mut journal = create(&mut sim, Durability::Ordered).await;
    for position in 0..16 {
        journal = put(&mut sim, journal, &[(position, 1, 100)]).await;
    }
    journal = raise_floor(&mut sim, journal, 8, 16).await;
    drop(journal);
    sim.simulate_crash_for_process(ip(), true);
    let (mut journal, recovery) = reopen(&mut sim, Durability::Ordered).await.expect("reopen");
    assert!(recovery.spare_kept, "{at}: {recovery:?}");
    for position in 17..24 {
        journal = put(&mut sim, journal, &[(position, 1, 100)]).await;
    }

    // The batch that rolls onto the spare dies between the rename and the
    // directory sync; each name since the last sync comes back or goes
    // away on its own coin.
    let mut config = StorageConfiguration::fast_local();
    config.unsynced_dir_entry_loss_probability = 0.5;
    sim.set_storage_config(config);
    moonpool_buggify::set_random_source(|| 0.0);
    moonpool_buggify::buggify_init(1.0);
    moonpool_buggify::hint::set_sink(Box::new(KillAtRename));
    let handle = tokio::spawn(async move {
        let mut batch = Batch::new();
        batch.put(24, id(24, 1), payload(24, 1, 100));
        let _ = journal.commit(batch).await;
    });
    for _ in 0..1000 {
        if handle.is_finished() {
            break;
        }
        while sim.pending_event_count() > 0 {
            sim.step();
        }
        tokio::task::yield_now().await;
    }
    moonpool_buggify::hint::clear_sink();
    moonpool_buggify::buggify_reset();
    assert!(!handle.is_finished(), "{at}: the roll reached its rename");
    handle.abort();
    let _ = handle.await;
    sim.simulate_crash_for_process(ip(), true);
    sim.set_storage_config(StorageConfiguration::fast_local());

    // A second roll adopts whatever spare the open kept.
    let (mut journal, _) = reopen(&mut sim, Durability::Ordered)
        .await
        .unwrap_or_else(|error| panic!("{at}: reopen after the cut: {error}"));
    for position in 25..42 {
        journal = put(&mut sim, journal, &[(position, 1, 100)]).await;
    }
    drop(journal);
    sim.simulate_crash_for_process(ip(), true);
    let (mut journal, _) = reopen(&mut sim, Durability::Ordered)
        .await
        .unwrap_or_else(|error| panic!("{at}: reopen after the second roll: {error}"));
    for position in (8..24).chain(25..42) {
        let (back, bytes) = read(&mut sim, journal, position).await;
        journal = back;
        assert_eq!(
            bytes.unwrap_or_else(|error| panic!("{at}: position {position}: {error}")),
            payload(position, 1, 100),
            "{at}: position {position}"
        );
    }
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

#[test]
fn a_batch_past_one_segment_is_refused_and_fits_on_split() {
    runtime().block_on(async {
        let mut sim = sim(16);
        let geometry = Geometry::small();
        // 100 entries of 1 KiB: more than the small shape's 64 KiB entry log.
        let puts: Vec<(u64, Vec<u8>)> = (0..100)
            .map(|p| (p, vec![u8::try_from(p).unwrap_or(0); 1024]))
            .collect();
        let mut whole = Batch::new();
        for (position, payload) in &puts {
            whole.put(*position, id(*position, 1), payload.clone());
        }
        assert!(!whole.fits(geometry), "the whole batch overflows a segment");
        let journal = create(&mut sim, Durability::Ordered).await;
        let refused = commit(&mut sim, journal, whole).await;
        assert!(
            matches!(refused, Err(CommitError::BatchTooLarge)),
            "{refused:?}"
        );
        // Refused before anything was written: the journal reopens empty,
        // and the same writes split by `fits` all commit.
        let (mut journal, _) = reopen(&mut sim, Durability::Ordered).await.expect("reopen");
        assert_eq!(journal.positions().count(), 0, "nothing was written");
        let mut chunks = 0;
        let mut chunk = Batch::new();
        for (position, payload) in &puts {
            if !chunk.fits_another(geometry, payload.len()) {
                assert!(chunk.fits(geometry));
                journal = commit(&mut sim, journal, std::mem::take(&mut chunk))
                    .await
                    .expect("a chunk that fits commits");
                chunks += 1;
            }
            chunk.put(*position, id(*position, 1), payload.clone());
        }
        journal = commit(&mut sim, journal, chunk)
            .await
            .expect("the last chunk commits");
        assert!(chunks >= 1, "the writes needed more than one commit");
        drop(journal);
        let (journal, _) = reopen(&mut sim, Durability::Ordered).await.expect("reopen");
        assert_eq!(journal.positions().count(), puts.len());
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

/// A replay reads runs of entries with one read each: over batches of many
/// entries, positions written out of order, overwrites, several segments
/// and a damaged entry inside a run, it answers what a read of each
/// position answers.
#[test]
fn a_replay_answers_what_each_read_answers() {
    runtime().block_on(async {
        for durability in BOTH {
            let mut sim = sim(8);
            let mut journal = create(&mut sim, durability).await;
            for round in 0..12u64 {
                let puts: Vec<(u64, u64, usize)> = (0..40u64)
                    .map(|at| {
                        (
                            (at * 7 + round * 3) % 97,
                            round + 1,
                            40 + usize::try_from(at % 9).expect("small") * 50,
                        )
                    })
                    .collect();
                journal = put(&mut sim, journal, &puts).await;
            }
            let damaged = journal.positions().nth(30).expect("a live position").0;
            let (_, entry) = spots(&journal, damaged);
            drop(journal);
            flip(&mut sim, entry.0, entry.1).await;
            let (journal, _) = reopen(&mut sim, durability).await.expect("opens");
            let positions: Vec<u64> = journal.positions().map(|(p, _)| p).collect();
            let (journal, replay) = run(&mut sim, move |_| async move {
                let replay = journal.replay(..).await;
                (journal, replay)
            })
            .await;
            let replay = replay.expect("replay");
            assert_eq!(
                replay.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
                positions,
                "every live position once, in order"
            );
            let mut journal = journal;
            for (position, replayed) in replay {
                let (back, read) = read(&mut sim, journal, position).await;
                journal = back;
                assert_eq!(
                    replayed
                        .map(|entry| entry.payload)
                        .map_err(|error| format!("{error:?}")),
                    read.map_err(|error| format!("{error:?}")),
                    "position {position} under {durability:?}"
                );
            }
            let (_, read) = read(&mut sim, journal, damaged).await;
            assert!(matches!(read, Err(ReadError::Damaged { .. })), "{read:?}");
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

/// A `Batched` last batch the metainfo does not record: a crash explains a
/// slot whose record and entry are both damaged (an `Ordered` batch with an
/// intact record does not, see
/// `two_faults_on_a_witnessed_last_batch_refuse_to_open`).
#[test]
fn a_torn_last_record_is_discarded_durably() {
    runtime().block_on(async {
        for durability in [Durability::Batched] {
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
        let before = names(&mut sim).await;
        // A peek and a create give the verdict of the open (#488). A create
        // that succeeded here would read the old segments as its own log.
        let peeked = run(&mut sim, |provider| async move {
            Journal::peek_meta(&provider, DIR, JOURNAL).await
        })
        .await;
        assert!(matches!(peeked, Err(OpenError::MetaLost)), "{peeked:?}");
        let created = run(&mut sim, |provider| async move {
            Journal::create(provider, DIR, JOURNAL, config(Durability::Ordered), b"new")
                .await
                .map(drop)
        })
        .await;
        assert!(matches!(created, Err(OpenError::MetaLost)), "{created:?}");
        assert_eq!(names(&mut sim).await, before, "nothing deleted");
        let refused = reopen(&mut sim, Durability::Ordered).await.map(drop);
        assert!(matches!(refused, Err(OpenError::MetaLost)), "{refused:?}");
    });
}

/// A spare, or a file that a crashed creation left, is not a journal:
/// `create` deletes it and succeeds, and the new journal reads empty.
#[test]
fn create_deletes_a_spare_or_a_leftover_and_succeeds() {
    runtime().block_on(async {
        for leftover in [
            "spare-0000000009-0000000000.wal",
            "seg-00000000000000000001.wal.tmp",
            "meta.tmp",
        ] {
            let mut sim = sim(19);
            run(&mut sim, move |provider| async move {
                provider.create_dir_all(DIR).await?;
                let file = provider
                    .open(&format!("{DIR}/{leftover}"), OpenOptions::create_write())
                    .await?;
                file.write_at(0, &[0xAB; 4096]).await?;
                file.sync_all().await
            })
            .await
            .expect("write a leftover");
            let peeked = run(&mut sim, |provider| async move {
                Journal::peek_meta(&provider, DIR, JOURNAL).await
            })
            .await
            .expect("peek");
            assert_eq!(peeked, None, "{leftover}");
            let journal = create(&mut sim, Durability::Ordered).await;
            assert_eq!(journal.positions().count(), 0, "{leftover}");
            drop(journal);
            assert_eq!(
                names(&mut sim).await,
                vec!["meta".to_string()],
                "{leftover}"
            );
            let (journal, _) = reopen(&mut sim, Durability::Ordered).await.expect("reopen");
            assert_eq!(journal.positions().count(), 0, "{leftover}");
        }
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
    assert!(
        tally.onto_spare > 0,
        "no commit ever rolled onto a recycled segment"
    );
    assert!(tally.spares_kept > 0, "no open ever kept a recycled spare");
    assert!(
        tally.blocks_repaired > 0,
        "no open or scrub ever wrote a rebuilt record back"
    );
}

/// The crash loop under moonpool's full crash physics, for one durability
/// setting (`None` draws it afresh every round), over `count` seeds unless
/// `JOURNAL_CRASH_SEEDS` sets another count.
fn harsh(durability: Option<Durability>, count: u64) {
    let mut tally = support::Tally::default();
    for seed in seeds(count) {
        crash_loop(seed, CrashModel::Harsh, durability, &mut tally);
    }
    eprintln!("harsh {durability:?}: {tally:?}");
    assert!(tally.torn > 0, "no crash ever tore a record");
    assert!(tally.prefix_dropped > 0, "no segment was ever freed");
    assert!(
        tally.onto_spare > 0,
        "no commit ever rolled onto a recycled segment"
    );
    assert!(tally.spares_kept > 0, "no open ever kept a recycled spare");
    assert!(
        tally.blocks_repaired > 0,
        "no open or scrub ever wrote a rebuilt record back"
    );
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
    harsh(Some(Durability::Ordered), 120);
}

#[test]
fn batched_under_moonpools_full_physics_nothing_acknowledged_is_lost_or_damaged() {
    // A torn record under Batched needs a crash to damage a record and its
    // entry, both written since the last sync: about one seed in 60 finds
    // one. 120 seeds found from 0 to 7, so a change anywhere in the loop
    // could leave none. 480 seeds find about 8.
    harsh(Some(Durability::Batched), 480);
}

#[test]
fn mixed_under_moonpools_full_physics_nothing_acknowledged_is_lost_or_damaged() {
    harsh(None, 120);
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

#[test]
fn a_roll_on_a_full_disk_fails_as_storage_full_and_leaves_no_segment_behind() {
    runtime().block_on(async {
        let mut sim = sim(11);
        let mut journal = create(&mut sim, Durability::Ordered).await;
        journal = put(&mut sim, journal, &[(0, 1, 100)]).await;
        // Room for what the journal holds now, and not for a second segment.
        sim.set_disk_capacity(ip(), Some(moonpool_sim::DiskCapacity::new(u64::MAX)));
        let used = sim
            .disk_space(ip())
            .map(|space| u64::MAX - space.free_bytes);
        let used = used.expect("a disk with a capacity reports its space");
        sim.set_disk_capacity(ip(), Some(moonpool_sim::DiskCapacity::new(used + 4096)));
        let before = names(&mut sim).await;

        // Each batch takes a fresh persist block: eight per segment.
        for position in 1..8 {
            journal = put(&mut sim, journal, &[(position, 1, 100)]).await;
        }
        let mut batch = Batch::new();
        batch.put(8, id(8, 1), payload(8, 1, 100));
        let Err(CommitError::Io { error, .. }) = commit(&mut sim, journal, batch).await else {
            panic!("the roll must fail on a full disk");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::StorageFull);
        assert_eq!(
            names(&mut sim).await,
            before,
            "a failed roll leaves no segment, whole or half-written"
        );

        // The disk gets room: the reopened journal rolls and keeps every
        // batch committed before the refusal.
        sim.set_disk_capacity(ip(), None);
        let (journal, _) = reopen(&mut sim, Durability::Ordered).await.expect("reopen");
        let journal = put(&mut sim, journal, &[(8, 1, 100)]).await;
        let live: Vec<u64> = journal.positions().map(|(p, _)| p).collect();
        assert_eq!(live, (0..9).collect::<Vec<_>>());
    });
}

/// Read every byte of `path`.
async fn file_bytes(sim: &mut SimWorld, path: String) -> Vec<u8> {
    run(sim, move |provider| async move {
        let file = provider.open(&path, OpenOptions::read_only()).await?;
        let len = usize::try_from(file.size().await?).expect("small");
        let mut bytes = vec![0; len];
        file.read_at(0, &mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    })
    .await
    .expect("read a file")
}

/// Write `bytes` over `path` from its start, durably.
async fn put_bytes(sim: &mut SimWorld, path: String, bytes: Vec<u8>) {
    run(sim, move |provider| async move {
        let file = provider.open(&path, OpenOptions::read_write()).await?;
        file.write_at(0, &bytes).await?;
        file.sync_all().await
    })
    .await
    .expect("write a file");
}

/// Five batches, then one last batch of two entries at 6 and 7, with new
/// metainfo when `witnessed`: the metainfo then records the batch.
async fn five_then_two(sim: &mut SimWorld, durability: Durability, witnessed: bool) -> J {
    let journal = five(sim, durability).await;
    let mut batch = Batch::new();
    batch
        .put(6, id(6, 1), payload(6, 1, 300))
        .put(7, id(7, 1), payload(7, 1, 300));
    if witnessed {
        batch.set_meta("after seven");
    }
    commit(sim, journal, batch).await.expect("commit")
}

/// The acknowledged last batch, a record and its entry both rotted: a
/// crash cannot explain it once the metainfo recorded the batch, or once
/// an `Ordered` batch shows one intact record (its entries were durable
/// before any record was written). The open refuses; it never forgets the
/// entry.
#[test]
fn two_faults_on_a_witnessed_last_batch_refuse_to_open() {
    runtime().block_on(async {
        for (durability, witnessed) in [
            (Durability::Ordered, false),
            (Durability::Ordered, true),
            (Durability::Batched, true),
        ] {
            let mut sim = sim(9);
            let journal = five_then_two(&mut sim, durability, witnessed).await;
            let (record, entry) = spots(&journal, 7);
            drop(journal);
            flip(&mut sim, record.0, record.1).await;
            flip(&mut sim, entry.0, entry.1).await;
            let refused = reopen(&mut sim, durability).await.map(drop);
            assert!(
                matches!(refused, Err(OpenError::DoubleFault { index: 1, .. })),
                "{durability:?}, witnessed {witnessed}: {refused:?}"
            );
        }
    });
}

/// An `Ordered` last batch torn by a crash: one record damaged beside its
/// intact entry, one slot with its record and entry both damaged. The open
/// rebuilds the first and discards the second in one rewrite, and a crash
/// that tears that rewrite can lose the discard while the rebuilt record
/// lands. The rebuilt record is no witness, so the next open discards the
/// slot again; it never refuses to open.
#[test]
fn a_rebuilt_record_beside_a_lost_discard_is_no_witness() {
    runtime().block_on(async {
        let mut sim = sim(22);
        let journal = five_then_two(&mut sim, Durability::Ordered, false).await;
        let (record_6, _) = spots(&journal, 6);
        let (record_7, entry_7) = spots(&journal, 7);
        drop(journal);
        flip(&mut sim, record_6.0.clone(), record_6.1).await;
        flip(&mut sim, record_7.0.clone(), record_7.1).await;
        flip(&mut sim, entry_7.0, entry_7.1).await;
        // The metainfo as the crash leaves it: an open raises its generation
        // only after the rewrite.
        let meta = file_bytes(&mut sim, format!("{DIR}/meta")).await;
        let (journal, recovery) = reopen(&mut sim, Durability::Ordered).await.expect("opens");
        assert_eq!((recovery.rebuilt, recovery.torn), (1, 1));
        assert_eq!(journal.state(7), State::Empty);
        drop(journal);
        // The torn rewrite: the rebuilt record landed, the discard was lost.
        put_bytes(&mut sim, format!("{DIR}/meta"), meta).await;
        flip(&mut sim, record_7.0, record_7.1).await;
        let (journal, recovery) = reopen(&mut sim, Durability::Ordered)
            .await
            .expect("a rebuilt record is no witness");
        assert_eq!((recovery.rebuilt, recovery.torn), (0, 1));
        assert_eq!(journal.state(7), State::Empty);
        let (journal, bytes) = read(&mut sim, journal, 6).await;
        assert_eq!(bytes.expect("intact"), payload(6, 1, 300));
        drop(journal);
        let (journal, recovery) = reopen(&mut sim, Durability::Ordered)
            .await
            .expect("reopens");
        assert_eq!(recovery.torn, 0, "the second open wrote its verdict down");
        let live: Vec<u64> = journal.positions().map(|(p, _)| p).collect();
        assert_eq!(live, vec![1, 2, 3, 4, 5, 6]);
    });
}

/// Every record and the first entry of an acknowledged last batch rotted:
/// nothing on disk says the batch was there, but the metainfo recorded it,
/// so the log ending before it is a lost batch, not the end.
#[test]
fn a_witnessed_last_batch_with_no_surviving_identity_is_lost_not_the_end() {
    runtime().block_on(async {
        for durability in BOTH {
            let mut sim = sim(9);
            let journal = five_then_two(&mut sim, durability, true).await;
            let (record_6, entry_6) = spots(&journal, 6);
            let (record_7, _) = spots(&journal, 7);
            drop(journal);
            flip(&mut sim, record_6.0, record_6.1).await;
            flip(&mut sim, record_7.0, record_7.1).await;
            flip(&mut sim, entry_6.0, entry_6.1).await;
            let refused = reopen(&mut sim, durability).await.map(drop);
            assert!(
                matches!(refused, Err(OpenError::LostBatch { .. })),
                "{durability:?}: {refused:?}"
            );
        }
    });
}

/// The metainfo of an earlier open restored in both copies (two lost copy
/// writes): the log holds a batch of a later open generation, which proves
/// the metainfo older than the log.
#[test]
fn a_metainfo_older_than_the_log_is_refused() {
    runtime().block_on(async {
        let mut sim = sim(17);
        let journal = five(&mut sim, Durability::Ordered).await;
        let mut batch = Batch::new();
        batch.set_meta("promise 3");
        drop(commit(&mut sim, journal, batch).await.expect("meta"));
        let (journal, _) = reopen(&mut sim, Durability::Ordered).await.expect("reopen");
        let older = file_bytes(&mut sim, format!("{DIR}/meta")).await;
        let journal = put(&mut sim, journal, &[(6, 1, 10)]).await;
        drop(journal);
        let (journal, _) = reopen(&mut sim, Durability::Ordered).await.expect("reopen");
        let mut batch = Batch::new();
        batch.set_meta("promise 4");
        let journal = commit(&mut sim, journal, batch).await.expect("meta");
        let journal = put(&mut sim, journal, &[(7, 1, 10)]).await;
        drop(journal);
        put_bytes(&mut sim, format!("{DIR}/meta"), older).await;
        let refused = reopen(&mut sim, Durability::Ordered).await.map(drop);
        assert!(
            matches!(refused, Err(OpenError::MetaRolledBack { .. })),
            "{refused:?}"
        );
    });
}

/// A persist record rotted after the open: nothing reads it again until
/// the next open, so a caller's scrub asks for it. A new put of the
/// position repairs it.
#[test]
fn a_scrub_finds_a_rotted_record_and_a_new_put_repairs_it() {
    runtime().block_on(async {
        for durability in BOTH {
            let mut sim = sim(18);
            let journal = five(&mut sim, durability).await;
            let (record, _) = spots(&journal, 3);
            let (journal, clean) = run(&mut sim, move |_| async move {
                let found = journal.verify_records(..).await;
                (journal, found)
            })
            .await;
            assert_eq!(clean.expect("verify"), Vec::<u64>::new());
            flip(&mut sim, record.0, record.1).await;
            let (journal, found) = run(&mut sim, move |_| async move {
                let found = journal.verify_records(2..5).await;
                (journal, found)
            })
            .await;
            assert_eq!(found.expect("verify"), vec![3], "{durability:?}");
            let (journal, outside) = run(&mut sim, move |_| async move {
                let found = journal.verify_records(4..).await;
                (journal, found)
            })
            .await;
            assert_eq!(outside.expect("verify"), Vec::<u64>::new());
            let journal = put(&mut sim, journal, &[(3, 2, 300)]).await;
            let journal = put(&mut sim, journal, &[(8, 1, 10)]).await;
            drop(journal);
            let (journal, recovery) = reopen(&mut sim, durability).await.expect("reopen");
            // The superseded record is still damaged on disk: the open
            // rebuilds it from its entry and writes it back, and the
            // position lives on in its new record.
            assert_eq!(recovery.rebuilt, 1, "{durability:?}");
            assert_eq!(recovery.repair.blocks_written, 1, "{durability:?}");
            let (journal, found) = run(&mut sim, move |_| async move {
                let found = journal.verify_records(..).await;
                (journal, found)
            })
            .await;
            assert_eq!(found.expect("verify"), Vec::<u64>::new());
            let (_, bytes) = read(&mut sim, journal, 3).await;
            assert_eq!(bytes.expect("intact"), payload(3, 2, 300));
        }
    });
}

/// Two batches: positions 1 and 2 in the first, 3 in the second, so the
/// first is not the last batch.
async fn two_then_one(sim: &mut SimWorld, durability: Durability) -> J {
    let journal = create(sim, durability).await;
    let journal = put(sim, journal, &[(1, 1, 300), (2, 1, 300)]).await;
    put(sim, journal, &[(3, 1, 300)]).await
}

async fn repair_records(sim: &mut SimWorld, mut journal: J) -> (J, RecordRepair) {
    run(sim, move |_| async move {
        let repair = journal.repair_records().await.expect("repair");
        (journal, repair)
    })
    .await
}

/// Probe `t7` of #482: a record rebuilt by an open is written back, so the
/// old slot of a position that a caller repaired is whole again, and a
/// later fault on its entry is one fault, not the second half of a double
/// fault.
#[test]
fn an_open_writes_back_a_rebuilt_record_of_an_earlier_batch() {
    runtime().block_on(async {
        for durability in BOTH {
            let mut sim = sim(19);
            let journal = two_then_one(&mut sim, durability).await;
            let (record, entry) = spots(&journal, 1);
            drop(journal);
            flip(&mut sim, record.0, record.1).await;
            let (journal, recovery) = reopen(&mut sim, durability).await.expect("opens");
            assert_eq!(recovery.rebuilt, 1, "{durability:?}");
            assert_eq!(
                recovery.repair,
                RecordRepair {
                    blocks_written: 1,
                    blocks_left: 0
                },
                "{durability:?}"
            );
            let (journal, found) = run(&mut sim, move |_| async move {
                let found = journal.verify_records(..).await;
                (journal, found)
            })
            .await;
            assert_eq!(found.expect("verify"), Vec::<u64>::new(), "{durability:?}");
            let journal = put(&mut sim, journal, &[(1, 2, 300)]).await;
            drop(journal);
            // The second fault: the old, replaced entry of position 1.
            flip(&mut sim, entry.0, entry.1).await;
            let (journal, recovery) = reopen(&mut sim, durability).await.expect("still opens");
            assert_eq!(recovery.rebuilt, 0, "{durability:?}");
            let (_, bytes) = read(&mut sim, journal, 1).await;
            assert_eq!(bytes.expect("intact"), payload(1, 2, 300));
        }
    });
}

/// A crash that tears the repair write can lose every record of the
/// block. Each entry is intact, so the next open rebuilds each record and
/// writes the block again.
#[test]
fn a_torn_repair_write_leaves_every_record_rebuildable() {
    runtime().block_on(async {
        for durability in BOTH {
            let mut sim = sim(20);
            let journal = two_then_one(&mut sim, durability).await;
            let (record, _) = spots(&journal, 1);
            drop(journal);
            // The image of a repair write of the first batch's block that
            // was torn to nothing: both records gone, both entries intact.
            zero(&mut sim, record.0, record.1 / 4096 * 4096, 4096).await;
            let (journal, recovery) = reopen(&mut sim, durability).await.expect("opens");
            assert_eq!(recovery.rebuilt, 2, "{durability:?}");
            assert_eq!(recovery.repair.blocks_written, 1, "{durability:?}");
            drop(journal);
            let (journal, recovery) = reopen(&mut sim, durability).await.expect("reopens");
            assert_eq!(recovery.rebuilt, 0, "the block was written whole");
            let live: Vec<u64> = journal.positions().map(|(p, _)| p).collect();
            assert_eq!(live, vec![1, 2, 3]);
            let mut journal = journal;
            for position in 1..=3 {
                let (back, bytes) = read(&mut sim, journal, position).await;
                journal = back;
                assert_eq!(bytes.expect("intact"), payload(position, 1, 300));
            }
        }
    });
}

/// A block whose rebuilt record sits beside a valid record with a damaged
/// entry is not written: that valid record is the only copy of the
/// damaged entry's identity.
#[test]
fn a_block_with_a_damaged_entry_is_not_written() {
    runtime().block_on(async {
        for durability in BOTH {
            let mut sim = sim(21);
            let journal = two_then_one(&mut sim, durability).await;
            let (record_1, _) = spots(&journal, 1);
            let (_, entry_2) = spots(&journal, 2);
            drop(journal);
            flip(&mut sim, record_1.0, record_1.1).await;
            flip(&mut sim, entry_2.0, entry_2.1).await;
            for _ in 0..2 {
                let (journal, recovery) = reopen(&mut sim, durability).await.expect("opens");
                assert_eq!(recovery.rebuilt, 1, "{durability:?}");
                assert_eq!(
                    recovery.repair,
                    RecordRepair {
                        blocks_written: 0,
                        blocks_left: 1
                    },
                    "{durability:?}"
                );
                let (_, bytes) = read(&mut sim, journal, 2).await;
                assert!(
                    matches!(bytes, Err(ReadError::Damaged { position: 2, id: got }) if got == id(2, 1)),
                    "{bytes:?}"
                );
            }
        }
    });
}

/// A scrub repairs what a caller cannot write again: the rotted record of
/// a replaced position and of a tombstone, on an open journal.
#[test]
fn a_scrub_repairs_the_records_of_replaced_positions_and_tombstones() {
    runtime().block_on(async {
        for durability in BOTH {
            let mut sim = sim(22);
            let journal = create(&mut sim, durability).await;
            let mut batch = Batch::new();
            batch.put(1, id(1, 1), payload(1, 1, 300)).clear(30..40);
            let journal = commit(&mut sim, journal, batch).await.expect("commit");
            let (record_1, entry_1) = spots(&journal, 1);
            // The tombstone is slot 1 of the batch: its record follows the
            // put's, its entry follows the put's 300-byte entry (368 bytes
            // with its header and padding): its last byte is 68 bytes on.
            let record_clear = (record_1.0.clone(), record_1.1 + 64);
            let entry_clear = (entry_1.0.clone(), entry_1.1 + 68);
            let journal = put(&mut sim, journal, &[(1, 2, 300)]).await;
            let journal = put(&mut sim, journal, &[(5, 1, 10)]).await;
            flip(&mut sim, record_1.0.clone(), record_1.1).await;
            flip(&mut sim, record_clear.0.clone(), record_clear.1).await;
            let (journal, found) = run(&mut sim, move |_| async move {
                let found = journal.verify_records(..).await;
                (journal, found)
            })
            .await;
            assert_eq!(
                found.expect("verify"),
                Vec::<u64>::new(),
                "neither record is of a live position"
            );
            let (journal, repair) = repair_records(&mut sim, journal).await;
            assert_eq!(
                repair,
                RecordRepair {
                    blocks_written: 1,
                    blocks_left: 0
                },
                "{durability:?}"
            );
            let (journal, again) = repair_records(&mut sim, journal).await;
            assert_eq!(again, RecordRepair::default(), "nothing is left to repair");
            drop(journal);
            // The second faults: both entries of the repaired slots.
            flip(&mut sim, entry_1.0, entry_1.1).await;
            flip(&mut sim, entry_clear.0, entry_clear.1).await;
            let (journal, recovery) = reopen(&mut sim, durability).await.expect("opens");
            assert_eq!(recovery.rebuilt, 0, "{durability:?}");
            let live: Vec<u64> = journal.positions().map(|(p, _)| p).collect();
            assert_eq!(live, vec![1, 5]);
        }
    });
}
