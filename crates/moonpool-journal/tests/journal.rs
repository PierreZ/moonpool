//! The journal over the simulator's storage: round trips, rollover,
//! truncation, tags, batched reads, the ambiguous-tail policy, metadata
//! repair, every row of the recovery table, and randomized crashes under two
//! fault models.

use std::future::Future;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_journal::{
    AmbiguousTail, Entry, EntryId, Geometry, Journal, JournalAtlas, JournalConfig, JournalError,
    JournalRegion, Record, Recovery, TAG_SIZE, Tag,
};
use moonpool_sim::{EioTarget, FaultFocus, SimStorageProvider, SimWorld, StorageConfiguration};

mod support;
use support::{Script, Scripted};

const DIR: &str = "wal";

fn ip() -> IpAddr {
    "10.0.0.1".parse().expect("valid IP")
}

/// A small geometry: a two-block slot table (128 slots), 16 KiB data start,
/// 128 KiB segments — rollover is cheap to reach.
fn small() -> JournalConfig {
    JournalConfig {
        geometry: Geometry {
            slot_count: 128,
            data_start: 16 * 1024,
            segment_size: 128 * 1024,
        },
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

/// The tag the tests write for `index` in `epoch`: every byte depends on
/// both, so a tag read back for the wrong entry never matches.
fn tag(index: u64, epoch: u64) -> Tag {
    let mut tag = [0; TAG_SIZE];
    tag[..8].copy_from_slice(&index.to_le_bytes());
    tag[8..16].copy_from_slice(&epoch.to_le_bytes());
    tag[16..].copy_from_slice(&(index ^ epoch.rotate_left(32)).to_le_bytes());
    tag
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
            .append(&[Record::new(epoch, &bytes).with_tag(tag(index, epoch))])
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

fn segment_path(first: u64) -> String {
    format!("{DIR}/seg-{first:020}.wal")
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

/// Overwrite `len` bytes at `offset` of `path` with zeros — a lost write.
async fn zero(sim: &mut SimWorld, path: String, offset: u64, len: usize) {
    run(sim, |provider| async move {
        let file = provider.open(&path, OpenOptions::read_write()).await?;
        file.write_at(offset, &vec![0; len]).await?;
        file.sync_all().await
    })
    .await
    .expect("zero a range");
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
                .map(|payload| Record::new(3, payload))
                .collect();
            assert_eq!(journal.append(&records).await?, 1..11);
            // A second batch lands right after the first, in the same block.
            append_each(&mut journal, 3, &[7, 9]).await?;
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
        assert_eq!(read.len(), 12);
        assert_eq!(
            recovery,
            Recovery::default(),
            "a clean reopen repairs nothing"
        );
    });
}

/// A nested journal directory survives a crash right after its first open:
/// every directory `open` created has a durable name, so neither the journal
/// nor its synced entries can vanish with an ancestor (moonpool#293).
#[test]
fn a_nested_journal_survives_a_crash_after_its_first_open() {
    const NESTED: &str = "a/b/c/wal";
    runtime().block_on(async {
        let mut sim = sim(3);
        sim.set_storage_config(StorageConfiguration {
            unsynced_dir_entry_loss_probability: 1.0,
            ..StorageConfiguration::fast_local()
        });
        let written = run(&mut sim, |provider| async move {
            let (mut journal, recovery) = Journal::open(provider, NESTED, small()).await?;
            assert!(recovery.created);
            append_each(&mut journal, 1, &[100, 200, 300]).await?;
            read_all(&journal).await
        })
        .await
        .expect("write");

        sim.simulate_crash_for_process(ip(), true);

        let (read, recovery) = run(&mut sim, |provider| async move {
            let (journal, recovery) = Journal::open(provider, NESTED, small()).await?;
            Ok::<_, JournalError>((read_all(&journal).await?, recovery))
        })
        .await
        .expect("reopen");
        assert!(!recovery.created, "the journal survived the crash");
        assert_eq!(read, written);
    });
}

#[test]
fn segments_are_found_by_listing_and_recovered_in_order() {
    runtime().block_on(async {
        let mut sim = sim(2);
        let lens: Vec<usize> = (0..300).map(|i| 50 + (i * 37) % 900).collect();
        let lens_again = lens.clone();
        run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider.clone(), small()).await?;
            append_each(&mut journal, 1, &lens[..150]).await?;
            for chunk in lens[150..].chunks(7) {
                let first = journal.next_index();
                let bytes: Vec<Vec<u8>> = chunk
                    .iter()
                    .zip(first..)
                    .map(|(len, index)| payload(index, *len))
                    .collect();
                let records: Vec<Record<'_>> = bytes
                    .iter()
                    .map(|payload| Record::new(2, payload))
                    .collect();
                journal.append(&records).await?;
            }
            // A creation a crash interrupted, and a file that is not ours.
            let leftover = format!("{}.tmp", segment_path(9999));
            drop(
                provider
                    .open(&leftover, OpenOptions::create_new_write())
                    .await?,
            );
            let foreign = format!("{DIR}/notes.txt");
            drop(
                provider
                    .open(&foreign, OpenOptions::create_new_write())
                    .await?,
            );
            Ok::<_, JournalError>(())
        })
        .await
        .expect("write");

        run(&mut sim, |provider| async move {
            let (journal, recovery) = open(provider.clone(), small()).await?;
            assert_eq!(recovery, Recovery::default());
            assert_eq!(journal.next_index(), 301);
            let entries = read_all(&journal).await?;
            for (entry, (index, len)) in entries.iter().zip((1..).zip(&lens_again)) {
                assert_eq!(entry.index, index);
                assert_eq!(entry.payload, payload(index, *len));
                assert_eq!(entry.epoch, if index <= 150 { 1 } else { 2 });
            }
            let names = provider.list_dir(DIR).await?;
            assert!(!names.iter().any(|name| name.contains(".tmp")), "{names:?}");
            assert!(
                names.contains(&"notes.txt".to_string()),
                "foreign files are left alone"
            );
            assert!(names.iter().filter(|name| name.starts_with("seg-")).count() > 1);
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
            append_each(&mut journal, 1, &[3000; 40]).await?; // two segments
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
            assert_eq!(
                provider.list_dir(DIR).await?,
                ["seg-00000000000000000001.wal"],
                "the segment past the cut is gone"
            );
            Ok::<_, JournalError>(())
        })
        .await
        .expect("reopen");
    });
}

#[test]
fn prefix_truncation_drops_whole_segments() {
    runtime().block_on(async {
        let mut sim = sim(4);
        let start = run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider.clone(), small()).await?;
            append_each(&mut journal, 1, &[3000; 100]).await?;
            journal.truncate_prefix(60).await?;
            let start = journal.start_index();
            assert!(start > 1 && start <= 60, "start {start}");
            assert!(!provider.exists(&segment_path(1)).await?);
            assert!(matches!(
                journal.read(start - 1).await,
                Err(JournalError::OutOfRange { .. })
            ));
            Ok::<_, JournalError>(start)
        })
        .await
        .expect("write");

        run(&mut sim, |provider| async move {
            let (journal, _) = open(provider, small()).await?;
            assert_eq!((journal.start_index(), journal.next_index()), (start, 101));
            let live = usize::try_from(101 - start).expect("small");
            assert_eq!(read_all(&journal).await?.len(), live);
            Ok::<_, JournalError>(())
        })
        .await
        .expect("reopen");
    });
}

/// The two ways a segment rolls over: its data region fills (one large
/// entry per batch, each on its own blocks) or its slot table does (128
/// small entries in one batch).
#[derive(Debug, Clone, Copy)]
enum Rollover {
    DataFull,
    TableFull,
}

/// Fill three segments the `shape` way, keep seg-1's bytes, and truncate
/// the prefix past the second segment's start: seg-1 and seg-2 are deleted.
/// Then put seg-1 back, byte for byte — what a crash leaves when the
/// unsynced unlink of seg-1 is undone and seg-2's is not. Returns the
/// segments' first indexes and the log's end.
async fn resurrect_the_first_segment(sim: &mut SimWorld, shape: Rollover) -> ([u64; 3], u64) {
    let (firsts, next, seg1) = run(sim, move |provider| async move {
        let (mut journal, _) = open(provider.clone(), small()).await?;
        let mut firsts = Vec::new();
        while firsts.len() < 3 {
            match shape {
                Rollover::DataFull => append_each(&mut journal, 1, &[3000]).await?,
                Rollover::TableFull => {
                    let next = journal.next_index();
                    let bytes: Vec<Vec<u8>> = (next..next + 128).map(|i| payload(i, 16)).collect();
                    let records: Vec<Record<'_>> = bytes
                        .iter()
                        .zip(next..)
                        .map(|(payload, index)| Record::new(1, payload).with_tag(tag(index, 1)))
                        .collect();
                    journal.append(&records).await?;
                }
            }
            let names = provider.list_dir(DIR).await?;
            firsts = names
                .iter()
                .filter_map(|name| {
                    name.strip_prefix("seg-")?
                        .strip_suffix(".wal")?
                        .parse()
                        .ok()
                })
                .collect();
            firsts.sort_unstable();
        }
        let file = provider
            .open(&segment_path(1), OpenOptions::read_only())
            .await?;
        let mut seg1 = vec![0; usize::try_from(file.size().await?).expect("small")];
        file.read_at(0, &mut seg1).await?;
        journal.truncate_prefix(firsts[2]).await?;
        assert_eq!(journal.start_index(), firsts[2]);
        Ok::<_, JournalError>((
            [firsts[0], firsts[1], firsts[2]],
            journal.next_index(),
            seg1,
        ))
    })
    .await
    .expect("write");
    run(sim, move |provider| async move {
        let file = provider
            .open(&segment_path(1), OpenOptions::create_write().read(true))
            .await?;
        file.write_at(0, &seg1).await?;
        file.sync_all().await?;
        provider.sync_dir(DIR).await
    })
    .await
    .expect("resurrect seg-1");
    (firsts, next)
}

/// A crash between `truncate_prefix`'s unlinks can bring the first dropped
/// segment back while the second stays deleted: `[seg-1, seg-3]`, a gap.
/// The new start is durable before any unlink, so opening knows seg-1 lies
/// wholly below it and deletes it again: every live entry reads back. Read
/// as a log, seg-1 would end past its real end and take the live log with
/// it (data-full), or index past its slot table (table-full).
#[test]
fn a_segment_resurrected_below_the_start_is_deleted_not_walked() {
    runtime().block_on(async {
        for (seed, shape) in [(40, Rollover::DataFull), (41, Rollover::TableFull)] {
            let mut sim = sim(seed);
            let (firsts, next) = resurrect_the_first_segment(&mut sim, shape).await;
            let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
            assert!(
                !recovery.torn_tail,
                "{shape:?}: nothing past the start is cut"
            );
            assert_eq!(
                (journal.start_index(), journal.next_index()),
                (firsts[2], next),
                "{shape:?}"
            );
            let live = run(&mut sim, |provider| async move {
                let entries = read_all(&journal).await?;
                Ok::<_, JournalError>((entries.len(), provider.exists(&segment_path(1)).await?))
            })
            .await
            .expect("read");
            assert_eq!(
                live,
                (usize::try_from(next - firsts[2]).expect("small"), false),
                "{shape:?}: every live entry reads, seg-1 is gone"
            );
        }
    });
}

/// A segment missing between the start and the tail is damage, never the
/// end of the log: opening refuses rather than delete what follows.
#[test]
fn a_missing_segment_is_refused_not_read_as_the_end() {
    runtime().block_on(async {
        let mut sim = sim(42);
        let firsts = run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider.clone(), small()).await?;
            append_each(&mut journal, 1, &[3000; 80]).await?;
            let mut firsts: Vec<u64> = provider
                .list_dir(DIR)
                .await?
                .iter()
                .filter_map(|name| {
                    name.strip_prefix("seg-")?
                        .strip_suffix(".wal")?
                        .parse()
                        .ok()
                })
                .collect();
            firsts.sort_unstable();
            provider.delete(&segment_path(firsts[1])).await?;
            provider.sync_dir(DIR).await?;
            Ok::<_, JournalError>(firsts)
        })
        .await
        .expect("write");
        assert!(firsts.len() >= 3);
        let opened = reopen(&mut sim).await.map(|_| ());
        assert!(
            matches!(opened, Err(JournalError::SegmentGap { end, next })
                if end == firsts[1] && next == firsts[2]),
            "{opened:?}"
        );
        let kept = run(&mut sim, move |provider| async move {
            provider.exists(&segment_path(firsts[2])).await
        })
        .await
        .expect("exists");
        assert!(kept, "nothing after the gap was deleted");
    });
}

#[test]
fn metadata_survives_damage_to_either_copy() {
    runtime().block_on(async {
        let mut sim = sim(5);
        run(&mut sim, |provider| async move {
            let (mut journal, recovery) = open(provider, small()).await?;
            assert_eq!(journal.meta(), None);
            assert!(!recovery.meta_repaired, "no metadata, nothing to repair");
            journal.save_meta(b"term=1 vote=a").await?;
            journal.save_meta(b"term=2 vote=b").await?;
            Ok::<_, JournalError>(())
        })
        .await
        .expect("write");
        // Both copies were rewritten: either one alone carries the newest,
        // and opening repairs the damaged one from its twin.
        flip(&mut sim, format!("{DIR}/meta.1"), 30).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(journal.meta(), Some(b"term=2 vote=b".as_slice()));
        assert!(recovery.meta_repaired);
        drop(journal);
        // So the other copy can fail next, and the value still stands.
        flip(&mut sim, format!("{DIR}/meta.0"), 30).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(journal.meta(), Some(b"term=2 vote=b".as_slice()));
        assert!(recovery.meta_repaired);
        drop(journal);
        let (_, recovery) = reopen(&mut sim).await.expect("reopen");
        assert!(!recovery.meta_repaired, "the repair is durable");
        // Both copies at once is beyond what two copies can survive.
        flip(&mut sim, format!("{DIR}/meta.0"), 30).await;
        flip(&mut sim, format!("{DIR}/meta.1"), 30).await;
        let opened = reopen(&mut sim).await.map(|_| ());
        assert!(
            matches!(opened, Err(JournalError::MetadataCorrupt { .. })),
            "{opened:?}"
        );
    });
}

/// A crash between the two copy writes leaves one copy a generation behind.
/// Opening brings it forward: otherwise a later fault in the newer copy
/// would silently roll the metadata back — for a Paxos acceptor, a promise
/// regressing below one it may have acted on.
#[test]
fn a_metadata_copy_left_behind_by_a_crash_is_brought_forward() {
    runtime().block_on(async {
        let mut sim = sim(14);
        let stale = run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider.clone(), small()).await?;
            journal.save_meta(b"promise=1").await?;
            let file = provider
                .open(&format!("{DIR}/meta.1"), OpenOptions::read_only())
                .await?;
            let mut stale = vec![0; usize::try_from(file.size().await?).expect("small")];
            file.read_at(0, &mut stale).await?;
            journal.save_meta(b"promise=2").await?;
            Ok::<_, JournalError>(stale)
        })
        .await
        .expect("write");
        // As if the crash hit after `meta.0` took generation 2 and before
        // `meta.1` did.
        run(&mut sim, |provider| async move {
            let file = provider
                .open(&format!("{DIR}/meta.1"), OpenOptions::read_write())
                .await?;
            file.write_at(0, &stale).await?;
            file.sync_all().await
        })
        .await
        .expect("roll meta.1 back");

        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(journal.meta(), Some(b"promise=2".as_slice()));
        assert!(recovery.meta_repaired, "the stale copy is rewritten");
        drop(journal);
        flip(&mut sim, format!("{DIR}/meta.0"), 30).await;
        let (journal, _) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(
            journal.meta(),
            Some(b"promise=2".as_slice()),
            "losing the newer copy no longer rolls the value back"
        );
    });
}

/// A `save_meta` that fails part-way — both copies renamed into place, the
/// directory sync after the second one failing — and a retry with a newer
/// value that the process dies in, after its first copy: the retry's value
/// is what reopening finds. Had the failed save not spent its generation,
/// the retry would have written that same generation and loading would
/// have had two valid copies of it to pick from, one holding the older
/// value — a promise rolled back.
#[test]
fn a_retried_metadata_save_outranks_the_one_that_failed() {
    runtime().block_on(async {
        let mut sim = sim(30);
        let script = Script::default();
        let armed = script.clone();
        run(&mut sim, |provider| async move {
            let provider = Scripted::new(provider, armed.clone());
            let (mut journal, _) = Journal::open(provider, DIR, small()).await?;
            journal.save_meta(b"promise=1").await?;
            armed.fail_sync_dir_after_rename_to("meta.1");
            assert!(journal.save_meta(b"promise=2").await.is_err());
            armed.refuse_rename_to("meta.1");
            assert!(journal.save_meta(b"promise=3").await.is_err());
            Ok::<_, JournalError>(())
        })
        .await
        .expect("write");
        drop(script);
        sim.simulate_crash_for_process(ip(), true);
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(journal.meta(), Some(b"promise=3".as_slice()));
        assert!(recovery.meta_repaired, "the copy left behind is rewritten");
    });
}

/// Two valid copies at one generation with different payloads cannot come
/// from a store, which spends its generation before writing either: opening
/// refuses them rather than pick one.
#[test]
fn two_copies_of_one_generation_that_disagree_are_refused() {
    runtime().block_on(async {
        let mut sim = sim(31);
        let other = run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider.clone(), small()).await?;
            journal.save_meta(b"promise=2").await?;
            let file = provider
                .open(&format!("{DIR}/meta.1"), OpenOptions::read_only())
                .await?;
            let mut bytes = vec![0; usize::try_from(file.size().await?).expect("small")];
            file.read_at(0, &mut bytes).await?;
            Ok::<_, JournalError>(bytes)
        })
        .await
        .expect("write");
        // Generation 1 again, holding another value, CRC and all.
        let mut forged = other;
        forged[24..33].copy_from_slice(b"promise=9");
        let crc = crc32c::crc32c_append(crc32c::crc32c(&forged[..20]), &forged[24..33]);
        forged[20..24].copy_from_slice(&crc.to_le_bytes());
        run(&mut sim, |provider| async move {
            let file = provider
                .open(&format!("{DIR}/meta.1"), OpenOptions::read_write())
                .await?;
            file.write_at(0, &forged).await?;
            file.sync_all().await
        })
        .await
        .expect("forge meta.1");
        let opened = reopen(&mut sim).await.map(|_| ());
        assert!(
            matches!(opened, Err(JournalError::MetadataCorrupt { .. })),
            "{opened:?}"
        );
    });
}

async fn peek(sim: &mut SimWorld) -> Result<Option<Vec<u8>>, JournalError> {
    run(sim, |provider| async move {
        Journal::peek_meta(&provider, DIR).await
    })
    .await
}

async fn read_copy(sim: &mut SimWorld) -> std::io::Result<Vec<u8>> {
    run(sim, |provider| async move {
        let file = provider
            .open(&format!("{DIR}/meta.0"), OpenOptions::read_only())
            .await?;
        let mut bytes = vec![0; usize::try_from(file.size().await?).expect("small")];
        file.read_at(0, &mut bytes).await?;
        Ok(bytes)
    })
    .await
}

/// `peek_meta` reads what `open` would load without repairing anything: one
/// damaged copy is still damaged afterwards (the next open repairs it),
/// both damaged is `MetadataCorrupt`, and an empty directory stays empty.
#[test]
fn peeking_at_the_metadata_changes_nothing() {
    runtime().block_on(async {
        let mut sim = sim(32);
        let peeked = run(&mut sim, |provider| async move {
            let before = Journal::peek_meta(&provider, DIR).await?;
            let listed = provider.exists(DIR).await?;
            Ok::<_, JournalError>((before, listed))
        })
        .await
        .expect("peek an empty disk");
        assert_eq!(peeked, (None, false), "nothing there, nothing created");

        run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider, small()).await?;
            journal.save_meta(b"formatted").await
        })
        .await
        .expect("write");
        assert_eq!(
            peek(&mut sim).await.expect("peek"),
            Some(b"formatted".to_vec())
        );

        flip(&mut sim, format!("{DIR}/meta.0"), 30).await;
        let damaged = read_copy(&mut sim).await.expect("read meta.0");
        assert_eq!(
            peek(&mut sim).await.expect("peek"),
            Some(b"formatted".to_vec())
        );
        assert_eq!(
            read_copy(&mut sim).await.expect("read meta.0"),
            damaged,
            "peeking repairs nothing"
        );
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert!(recovery.meta_repaired, "the damage was left for the open");
        drop(journal);

        flip(&mut sim, format!("{DIR}/meta.0"), 30).await;
        flip(&mut sim, format!("{DIR}/meta.1"), 30).await;
        let both = peek(&mut sim).await;
        assert!(
            matches!(both, Err(JournalError::MetadataCorrupt { .. })),
            "{both:?}"
        );
    });
}

/// `read_range` returns exactly what one `read` per index returns — across
/// segment boundaries, with a corrupt entry reported in place by identity —
/// and refuses a range outside the live log.
#[test]
fn a_batched_read_matches_one_read_per_entry() {
    runtime().block_on(async {
        let mut sim = sim(15);
        run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider, small()).await?;
            let lens: Vec<usize> = (0..120).map(|i| 10 + (i * 53) % 3000).collect();
            append_each(&mut journal, 4, &lens).await?;
            Ok::<_, JournalError>(())
        })
        .await
        .expect("write");
        // Entry 2's payload starts 64 bytes into it.
        let at = entry_offset(&mut sim, 2).await + 70;
        flip(&mut sim, segment_path(1), at).await;

        run(&mut sim, |provider| async move {
            let (journal, recovery) = open(provider.clone(), small()).await?;
            assert_eq!(recovery.corrupt.len(), 1);
            let (start, next) = (journal.start_index(), journal.next_index());
            let names = provider.list_dir(DIR).await?;
            assert!(names.len() > 1, "the log spans several segments: {names:?}");

            let batched = journal.read_range(start..next).await?;
            assert_eq!(batched.len(), usize::try_from(next - start).expect("small"));
            for (index, got) in (start..).zip(&batched) {
                match (journal.read(index).await, got) {
                    (Ok(one), Ok(many)) => {
                        assert_eq!(&one, many);
                        assert_eq!(many.tag, tag(index, 4));
                    }
                    (Err(JournalError::Corrupt(one)), Err(many)) => assert_eq!(one, *many),
                    (one, many) => panic!("index {index}: {one:?} vs {many:?}"),
                }
            }
            assert_eq!(batched[1].as_ref().err().map(|id| id.index), Some(2));

            // Sub-ranges and the empty range agree too.
            let middle = journal.read_range(start + 50..start + 70).await?;
            assert_eq!(middle, batched[50..70]);
            assert!(journal.read_range(next..next).await?.is_empty());
            for bad in [start + 1..next + 1, start + 5..start + 4] {
                assert!(matches!(
                    journal.read_range(bad).await,
                    Err(JournalError::OutOfRange { .. })
                ));
            }
            Ok::<_, JournalError>(())
        })
        .await
        .expect("read back");
    });
}

fn keep() -> JournalConfig {
    JournalConfig {
        ambiguous_tail: AmbiguousTail::Keep,
        ..small()
    }
}

/// Under `AmbiguousTail::Keep` the ambiguous last entry stays in the log,
/// marked corrupt, and is reported on every reopen until the caller
/// resolves it — its identity is never lost to a local truncation.
#[test]
fn a_kept_ambiguous_last_entry_survives_reopen_until_resolved() {
    runtime().block_on(async {
        let mut sim = sim(16);
        five_entries(&mut sim).await;
        let at = payload_byte(&mut sim, 5).await;
        flip(&mut sim, segment_path(1), at).await;
        let ambiguous = EntryId {
            index: 5,
            epoch: 7,
            tag: tag(5, 7),
        };
        for _ in 0..2 {
            let (journal, recovery) = run(&mut sim, |provider| async move {
                Journal::open(provider, DIR, keep()).await
            })
            .await
            .expect("reopen");
            assert_eq!(recovery.ambiguous_batch, vec![ambiguous]);
            assert!(recovery.corrupt.is_empty());
            assert!(!recovery.torn_tail, "nothing was truncated");
            assert_eq!(journal.next_index(), 6, "the entry is kept");
            run(&mut sim, |_| async move {
                assert!(matches!(
                    journal.read(5).await,
                    Err(JournalError::Corrupt(id)) if id == ambiguous
                ));
            })
            .await;
        }

        // Writing past it makes it an ordinary corrupt entry.
        run(&mut sim, |provider| async move {
            let (mut journal, _) = Journal::open(provider, DIR, keep()).await?;
            append_each(&mut journal, 8, &[64]).await
        })
        .await
        .expect("append");
        let (mut journal, recovery) = run(&mut sim, |provider| async move {
            Journal::open(provider, DIR, keep()).await
        })
        .await
        .expect("reopen");
        assert_eq!(recovery.corrupt, vec![ambiguous], "a later batch covers it");
        assert!(recovery.ambiguous_batch.is_empty());

        // The caller resolves it — here, as not committed — by truncating.
        run(&mut sim, |_| async move {
            journal.truncate_suffix(5).await.expect("truncate");
        })
        .await;
        let (journal, recovery) = run(&mut sim, |provider| async move {
            Journal::open(provider, DIR, keep()).await
        })
        .await
        .expect("reopen");
        assert_eq!(recovery, Recovery::default());
        assert_eq!(journal.next_index(), 5);
    });
}

/// Where entry `index` of the closed journal starts, as its slot records
/// it: every batch starts on a fresh block, so the offsets depend on how the
/// entries were batched, and the slot table is what knows.
async fn entry_offset(sim: &mut SimWorld, index: u64) -> u64 {
    run(sim, |provider| async move {
        JournalAtlas::scan(&provider, DIR, small().geometry).await
    })
    .await
    .expect("scan")
    .entry(index)
    .expect("a live entry")
    .layout
    .bytes
    .start
}

/// A byte inside entry `index`'s payload (64-byte header first).
async fn payload_byte(sim: &mut SimWorld, index: u64) -> u64 {
    entry_offset(sim, index).await + 80
}

/// Slot `i` of the first segment.
fn slot_offset(index: u64) -> u64 {
    8192 + (index - 1) * 64
}

async fn five_entries(sim: &mut SimWorld) {
    run(sim, |provider| async move {
        let (mut journal, _) = open(provider, small()).await?;
        append_each(&mut journal, 7, &[64; 5]).await
    })
    .await
    .expect("write");
}

async fn reopen(
    sim: &mut SimWorld,
) -> Result<(Journal<SimStorageProvider>, Recovery), JournalError> {
    run(sim, |provider| async move { open(provider, small()).await }).await
}

/// Row "bad entry, valid slot": mid-log, the entry is marked corrupt and
/// reported with its epoch; nothing after it is lost.
#[test]
fn a_damaged_entry_mid_log_is_reported_not_truncated() {
    runtime().block_on(async {
        let mut sim = sim(6);
        five_entries(&mut sim).await;
        let at = payload_byte(&mut sim, 3).await;
        flip(&mut sim, segment_path(1), at).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        let damaged = EntryId {
            index: 3,
            epoch: 7,
            tag: tag(3, 7),
        };
        assert_eq!(recovery.corrupt, vec![damaged], "reported with its tag");
        assert!(recovery.ambiguous_batch.is_empty());
        assert_eq!(journal.next_index(), 6, "nothing after the damage is lost");
        assert_eq!(journal.entry_id(3), Some(damaged));
        run(&mut sim, |_| async move {
            assert!(matches!(
                journal.read(3).await,
                Err(JournalError::Corrupt(id)) if id == damaged
            ));
            let intact = journal.read(4).await.expect("intact");
            assert_eq!(intact.payload, payload(4, 64));
        })
        .await;
    });
}

/// Row "good entry, bad slot": the slot is rebuilt from the entry.
#[test]
fn damaged_slots_are_rebuilt_from_intact_entries() {
    runtime().block_on(async {
        let mut sim = sim(7);
        five_entries(&mut sim).await;
        for index in 1..=5 {
            flip(&mut sim, segment_path(1), slot_offset(index) + 9).await;
        }
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(recovery.slots_rewritten, 5);
        assert!(recovery.corrupt.is_empty());
        assert_eq!(journal.next_index(), 6);
        assert_eq!(
            journal.entry_id(4).map(|id| id.tag),
            Some(tag(4, 7)),
            "a rebuilt slot takes its tag from the entry"
        );
        drop(journal);
        let (_, recovery) = reopen(&mut sim).await.expect("second reopen");
        assert_eq!(
            recovery,
            Recovery::default(),
            "the rebuilt slots are durable"
        );
    });
}

/// Row "good entry, empty slot": the identifier write was lost, the entry
/// was not — keep it and rewrite the slot.
#[test]
fn a_lost_slot_write_is_rewritten() {
    runtime().block_on(async {
        let mut sim = sim(8);
        five_entries(&mut sim).await;
        zero(&mut sim, segment_path(1), slot_offset(4), 128).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(recovery.slots_rewritten, 2);
        assert_eq!(journal.next_index(), 6);
    });
}

/// Row "bad entry, bad slot": nothing identifies what was there; refuse.
#[test]
fn a_double_fault_refuses_to_start() {
    runtime().block_on(async {
        let mut sim = sim(9);
        five_entries(&mut sim).await;
        let at = payload_byte(&mut sim, 3).await;
        flip(&mut sim, segment_path(1), slot_offset(3) + 9).await;
        flip(&mut sim, segment_path(1), at).await;
        let opened = reopen(&mut sim).await.map(|_| ());
        assert!(
            matches!(opened, Err(JournalError::DoubleFault { index: 3 })),
            "{opened:?}"
        );
    });
}

/// Row "bad entry, empty slot": a torn tail. The log ends there, and what
/// lies past the end is zeroed before anything is appended.
#[test]
fn a_torn_tail_is_truncated_and_scrubbed() {
    runtime().block_on(async {
        let mut sim = sim(10);
        five_entries(&mut sim).await;
        let at = payload_byte(&mut sim, 4).await;
        zero(&mut sim, segment_path(1), slot_offset(4), 128).await;
        flip(&mut sim, segment_path(1), at).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(
            journal.next_index(),
            4,
            "entry 4 and everything after it go"
        );
        assert!(recovery.torn_tail);
        assert!(recovery.ambiguous_batch.is_empty());
        drop(journal);
        // Entry 5 was intact but past the end: the scrub zeroed it, so it
        // can never come back through a lost slot.
        let (mut journal, recovery) = reopen(&mut sim).await.expect("second reopen");
        assert_eq!(recovery, Recovery::default());
        run(&mut sim, |_| async move {
            append_each(&mut journal, 8, &[64]).await.expect("append");
            assert_eq!(journal.read(4).await.expect("new entry").epoch, 8);
        })
        .await;
    });
}

/// The last entry with an intact slot and a bad entry is ambiguous: a single
/// node truncates it, and reports it for a replication layer to decide.
#[test]
fn the_ambiguous_last_entry_is_truncated_and_reported() {
    runtime().block_on(async {
        let mut sim = sim(11);
        five_entries(&mut sim).await;
        let at = payload_byte(&mut sim, 5).await;
        flip(&mut sim, segment_path(1), at).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(
            recovery.ambiguous_batch,
            vec![EntryId {
                index: 5,
                epoch: 7,
                tag: tag(5, 7)
            }]
        );
        assert!(recovery.corrupt.is_empty());
        assert_eq!(journal.next_index(), 5);
    });
}

/// Entries 1 and 2 as single-entry batches, then 3..=5 as one batch of
/// three, all with 64-byte payloads.
async fn two_batches_then_three(sim: &mut SimWorld) {
    run(sim, |provider| async move {
        let (mut journal, _) = open(provider, small()).await?;
        append_each(&mut journal, 7, &[64; 2]).await?;
        let bytes: Vec<Vec<u8>> = (3..=5).map(|index| payload(index, 64)).collect();
        let records: Vec<Record<'_>> = bytes
            .iter()
            .zip(3..)
            .map(|(payload, index)| Record::new(7, payload).with_tag(tag(index, 7)))
            .collect();
        journal.append(&records).await.map(|_| ())
    })
    .await
    .expect("write");
}

fn id(index: u64) -> EntryId {
    EntryId {
        index,
        epoch: 7,
        tag: tag(index, 7),
    }
}

/// A crash before a batch's sync can tear any of its entries, not only the
/// last: every damaged entry of the last batch is ambiguous, none is
/// reported as corruption of acknowledged data. `Truncate` cuts at the
/// first of them, taking the batch's intact entries after it along.
#[test]
fn every_damaged_entry_of_the_last_batch_is_ambiguous() {
    runtime().block_on(async {
        let mut sim = sim(17);
        two_batches_then_three(&mut sim).await;
        let at = payload_byte(&mut sim, 3).await;
        flip(&mut sim, segment_path(1), at).await;
        let at = payload_byte(&mut sim, 4).await;
        flip(&mut sim, segment_path(1), at).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(recovery.ambiguous_batch, vec![id(3), id(4)]);
        assert!(recovery.corrupt.is_empty(), "{recovery:?}");
        assert!(recovery.torn_tail);
        assert_eq!(
            journal.next_index(),
            3,
            "the batch goes from its first damage"
        );
    });
}

/// The same damage under `Keep`: nothing is truncated, the intact entry
/// after the damage is still readable, and the identities survive reopen.
#[test]
fn a_kept_ambiguous_batch_keeps_its_intact_entries() {
    runtime().block_on(async {
        let mut sim = sim(18);
        two_batches_then_three(&mut sim).await;
        let at = payload_byte(&mut sim, 3).await;
        flip(&mut sim, segment_path(1), at).await;
        let at = payload_byte(&mut sim, 4).await;
        flip(&mut sim, segment_path(1), at).await;
        for _ in 0..2 {
            let (journal, recovery) = run(&mut sim, |provider| async move {
                Journal::open(provider, DIR, keep()).await
            })
            .await
            .expect("reopen");
            assert_eq!(recovery.ambiguous_batch, vec![id(3), id(4)]);
            assert!(recovery.corrupt.is_empty());
            assert!(!recovery.torn_tail);
            assert_eq!(journal.next_index(), 6);
            run(&mut sim, |_| async move {
                let read = journal.read_range(3..6).await.expect("read");
                assert_eq!(read[0], Err(id(3)));
                assert_eq!(read[1], Err(id(4)));
                assert_eq!(
                    read[2].as_ref().map(|e| e.payload.clone()),
                    Ok(payload(5, 64))
                );
            })
            .await;
        }
    });
}

/// Damage to an entry of an *earlier* batch is corruption — a later sync
/// covers it — even when the last batch is damaged too.
#[test]
fn damage_before_the_last_batch_stays_corruption() {
    runtime().block_on(async {
        let mut sim = sim(19);
        two_batches_then_three(&mut sim).await;
        let at = payload_byte(&mut sim, 2).await;
        flip(&mut sim, segment_path(1), at).await;
        let at = payload_byte(&mut sim, 4).await;
        flip(&mut sim, segment_path(1), at).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(recovery.corrupt, vec![id(2)]);
        assert_eq!(recovery.ambiguous_batch, vec![id(4)]);
        assert_eq!(journal.next_index(), 4);
    });
}

/// The batch-start flag lives in the entry too, so a lost slot of the
/// batch's first entry is rebuilt with it and the batch is still found.
#[test]
fn a_rebuilt_slot_keeps_the_batch_boundary() {
    runtime().block_on(async {
        let mut sim = sim(20);
        two_batches_then_three(&mut sim).await;
        let at = payload_byte(&mut sim, 4).await;
        zero(&mut sim, segment_path(1), slot_offset(3), 64).await;
        flip(&mut sim, segment_path(1), at).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(recovery.slots_rewritten, 1);
        assert_eq!(recovery.ambiguous_batch, vec![id(4)]);
        assert!(recovery.corrupt.is_empty());
        assert_eq!(journal.next_index(), 4);
    });
}

/// A disk that loses every sector it was writing when the crash hits, and
/// fills a lost sector with zeros.
fn losing_disk(seed: u64) -> SimWorld {
    let mut sim = SimWorld::new_with_seed(seed);
    let mut config = StorageConfiguration::fast_local();
    config.clean_crash_probability = 0.0;
    config.correlated_rollback_probability = 0.0;
    config.crash_lost_probability = 1.0;
    config.garbage_fill_probability = 0.0;
    sim.set_storage_config(config);
    sim
}

/// Append batch A (three entries), then start batch B (one small entry) and
/// stop the world `stop_after` steps into B's append — or let it finish when
/// `None`. Returns how many steps B's append took, and the journal if it
/// finished.
async fn a_then_b(sim: &mut SimWorld, stop_after: Option<usize>) -> usize {
    let journal = run(sim, |provider| async move {
        let (mut journal, _) = open(provider, small()).await?;
        let bytes: Vec<Vec<u8>> = (1..=3).map(|index| payload(index, 64)).collect();
        let records: Vec<Record<'_>> = bytes
            .iter()
            .zip(1..)
            .map(|(payload, index)| Record::new(7, payload).with_tag(tag(index, 7)))
            .collect();
        journal.append(&records).await?;
        Ok::<_, JournalError>(journal)
    })
    .await
    .expect("batch A");
    let handle = tokio::spawn(async move {
        let mut journal = journal;
        append_each(&mut journal, 7, &[64]).await
    });
    let mut steps = 0;
    while !handle.is_finished() {
        if stop_after == Some(steps) {
            handle.abort();
            break;
        }
        if sim.pending_event_count() > 0 {
            sim.step();
            steps += 1;
        }
        tokio::task::yield_now().await;
    }
    let _ = handle.await;
    steps
}

/// An append never rewrites a sector an earlier sync made durable. Batch B
/// would fit in the block holding batch A's last entry; it starts on a
/// fresh block instead, so a crash that loses every sector B was writing —
/// its entry and its slot, which shares a slot-table block with A's — cannot
/// reach A's entries: A's lost slots are rebuilt from them.
#[test]
fn a_crash_during_an_append_cannot_reach_the_batch_before_it() {
    runtime().block_on(async {
        // Count the steps of B's append on one world, then replay the same
        // seed on another and crash one step short: B's writes have landed,
        // its sync has not.
        let steps = a_then_b(&mut losing_disk(21), None).await;
        let mut sim = losing_disk(21);
        a_then_b(&mut sim, Some(steps - 1)).await;
        sim.simulate_crash_for_process(ip(), true);
        let lost: usize = sim
            .take_storage_crash_reports()
            .iter()
            .filter(|report| report.path.starts_with(&format!("{DIR}/seg-")))
            .map(|report| report.resolutions.len())
            .sum();
        assert!(lost > 0, "the crash lost the sectors B was writing");

        let (journal, recovery) = reopen(&mut sim).await.expect("A survives");
        assert!(recovery.corrupt.is_empty(), "{recovery:?}");
        assert!(recovery.ambiguous_batch.is_empty(), "{recovery:?}");
        assert_eq!(journal.next_index(), 4, "B is gone, A is whole");
        run(&mut sim, |_| async move {
            for index in 1..=3 {
                let entry = journal.read(index).await.expect("A's entry");
                assert_eq!(entry.payload, payload(index, 64));
            }
        })
        .await;
    });
}

/// Where the slot of entry `index` of the closed journal lives.
async fn slot_at(sim: &mut SimWorld, index: u64) -> u64 {
    run(sim, |provider| async move {
        JournalAtlas::scan(&provider, DIR, small().geometry).await
    })
    .await
    .expect("scan")
    .slot(index)
    .expect("a live slot")
    .layout
    .bytes
    .start
}

/// Every slot is formatted with a reserved record naming its own index, so
/// a synced slot that comes back zeroed is damage, never "nothing here".
/// Beside an intact entry it is rebuilt and nothing is cut; beside a damaged
/// entry before the last batch it is a double fault — where reading zeros
/// as "never written" would silently end the log there and drop every
/// acknowledged entry after it.
#[test]
fn a_zeroed_slot_is_damage_never_the_end_of_the_log() {
    runtime().block_on(async {
        let mut sim = sim(22);
        five_entries(&mut sim).await;
        let at = slot_at(&mut sim, 2).await;
        zero(&mut sim, segment_path(1), at, 64).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(recovery.slots_rewritten, 1);
        assert!(!recovery.torn_tail);
        assert_eq!(journal.next_index(), 6, "nothing is cut");
        drop(journal);

        let mut sim = self::sim(23);
        five_entries(&mut sim).await;
        let slot = slot_at(&mut sim, 2).await;
        let entry = payload_byte(&mut sim, 2).await;
        zero(&mut sim, segment_path(1), slot, 64).await;
        flip(&mut sim, segment_path(1), entry).await;
        let opened = reopen(&mut sim)
            .await
            .map(|(journal, _)| journal.next_index());
        assert!(
            matches!(opened, Err(JournalError::DoubleFault { index: 2 })),
            "{opened:?}"
        );
    });
}

/// The last batch's entries and slots reach the disk through two unordered
/// writes and one sync, so a crash before that sync can damage both copies
/// of one identity. There that is a torn tail, not a double fault: the log
/// ends at the damage, everything before it intact, and the node boots.
#[test]
fn a_torn_identifier_in_the_last_batch_ends_the_log() {
    runtime().block_on(async {
        let mut sim = sim(24);
        two_batches_then_three(&mut sim).await;
        let slot = slot_at(&mut sim, 3).await;
        let entry = payload_byte(&mut sim, 3).await;
        flip(&mut sim, segment_path(1), slot + 9).await;
        flip(&mut sim, segment_path(1), entry).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("a torn last batch opens");
        assert!(recovery.torn_tail);
        assert!(recovery.corrupt.is_empty() && recovery.ambiguous_batch.is_empty());
        assert_eq!(journal.next_index(), 3, "the last batch is gone");
        run(&mut sim, |_| async move {
            for index in 1..=2 {
                let read = journal.read(index).await.expect("before it, intact");
                assert_eq!(read.payload, payload(index, 64));
            }
        })
        .await;
    });
}

/// A suffix cut in the middle of a block leaves that block alone — it holds
/// kept entries — and the discarded entries in it stay discarded: their
/// slots hold reserved records, and the walk never looks behind one.
#[test]
fn a_suffix_cut_mid_block_never_brings_the_cut_entries_back() {
    runtime().block_on(async {
        let mut sim = sim(25);
        run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider, small()).await?;
            let bytes: Vec<Vec<u8>> = (1..=5).map(|index| payload(index, 64)).collect();
            let records: Vec<Record<'_>> = bytes
                .iter()
                .zip(1..)
                .map(|(payload, index)| Record::new(7, payload).with_tag(tag(index, 7)))
                .collect();
            journal.append(&records).await?;
            journal.truncate_suffix(3).await
        })
        .await
        .expect("write and cut");
        for _ in 0..2 {
            let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
            assert_eq!(recovery, Recovery::default());
            assert_eq!(journal.next_index(), 3);
        }
        let (mut journal, _) = reopen(&mut sim).await.expect("reopen");
        run(&mut sim, |_| async move {
            append_each(&mut journal, 8, &[64]).await.expect("append");
        })
        .await;
        let (journal, _) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(journal.next_index(), 4);
        assert_eq!(journal.epoch(3), Some(8), "the new entry, not the cut one");
    });
}

/// The cut entries sit right where a continuation of the last kept entry
/// would, so the walk must not take them for one even when the reserved
/// slot after the cut is damaged: the last kept slot records the cut, and
/// the damaged identifier then ends the log as a torn tail.
#[test]
fn a_damaged_slot_past_a_cut_does_not_bring_the_cut_entry_back() {
    runtime().block_on(async {
        let mut sim = sim(26);
        run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider, small()).await?;
            let bytes: Vec<Vec<u8>> = (1..=5).map(|index| payload(index, 64)).collect();
            let records: Vec<Record<'_>> = bytes
                .iter()
                .zip(1..)
                .map(|(payload, index)| Record::new(7, payload).with_tag(tag(index, 7)))
                .collect();
            journal.append(&records).await?;
            journal.truncate_suffix(3).await
        })
        .await
        .expect("write and cut");
        flip(&mut sim, segment_path(1), slot_offset(3) + 9).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert!(recovery.torn_tail);
        assert_eq!(journal.next_index(), 3, "entry 3 stays cut");
    });
}

/// Append `count` image records as one checkpoint.
async fn checkpoint_of(
    journal: &mut Journal<SimStorageProvider>,
    count: u64,
) -> Result<std::ops::Range<u64>, JournalError> {
    let next = journal.next_index();
    let bytes: Vec<Vec<u8>> = (next..next + count)
        .map(|index| payload(index, 48))
        .collect();
    let records: Vec<Record<'_>> = bytes
        .iter()
        .zip(next..)
        .map(|(payload, index)| Record::new(7, payload).with_tag(tag(index, 7)))
        .collect();
    journal.append_checkpoint(&records).await
}

/// History, checkpoint C1 (entries 4–5), more history, checkpoint C2
/// (entries 8–10), and one more entry so C2 is not the last batch.
async fn two_checkpoints(sim: &mut SimWorld, config: JournalConfig) -> [std::ops::Range<u64>; 2] {
    run(sim, |provider| async move {
        let (mut journal, _) = Journal::open(provider, DIR, config).await?;
        append_each(&mut journal, 7, &[64; 3]).await?;
        let c1 = checkpoint_of(&mut journal, 2).await?;
        append_each(&mut journal, 7, &[64; 2]).await?;
        let c2 = checkpoint_of(&mut journal, 3).await?;
        append_each(&mut journal, 7, &[64]).await?;
        assert_eq!(journal.checkpoint(), Some(c2.clone()));
        Ok::<_, JournalError>([c1, c2])
    })
    .await
    .expect("write")
}

/// The newest intact checkpoint is where a replay starts. Damage one of
/// its entries and it is passed over for the one before it — reported
/// corrupt like any damaged batch — and with that one damaged too, there is
/// none, the whole history still live.
#[test]
fn a_damaged_checkpoint_is_passed_over_for_the_one_before_it() {
    runtime().block_on(async {
        let mut sim = sim(50);
        let [c1, c2] = two_checkpoints(&mut sim, small()).await;
        assert_eq!((c1.clone(), c2.clone()), (4..6, 8..11));
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(recovery.checkpoint, Some(c2.clone()));
        drop(journal);

        let at = payload_byte(&mut sim, 9).await;
        flip(&mut sim, segment_path(1), at).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(recovery.checkpoint, Some(c1.clone()));
        assert_eq!(
            recovery
                .corrupt
                .iter()
                .map(|id| id.index)
                .collect::<Vec<_>>(),
            [9]
        );
        drop(journal);

        let at = payload_byte(&mut sim, 4).await;
        flip(&mut sim, segment_path(1), at).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(recovery.checkpoint, None);
        assert_eq!(
            (journal.start_index(), journal.next_index()),
            (1, 12),
            "all history live"
        );
    });
}

/// A crash before a checkpoint's sync leaves it the last batch, handled as
/// any last batch is: here the disk loses every sector it was writing, so
/// the log ends where the checkpoint began. The one before it is named,
/// under either policy.
#[test]
fn a_crash_mid_checkpoint_leaves_the_one_before_it() {
    runtime().block_on(async {
        for config in [small(), keep()] {
            let run_until = |stop_after: Option<usize>| {
                let config = config.clone();
                async move {
                    let mut sim = losing_disk(51);
                    let journal = run(&mut sim, |provider| async move {
                        let (mut journal, _) = Journal::open(provider, DIR, config).await?;
                        append_each(&mut journal, 7, &[64; 2]).await?;
                        checkpoint_of(&mut journal, 2).await?;
                        append_each(&mut journal, 7, &[64]).await?;
                        Ok::<_, JournalError>(journal)
                    })
                    .await
                    .expect("write");
                    let handle = tokio::spawn(async move {
                        let mut journal = journal;
                        checkpoint_of(&mut journal, 4).await.map(|_| ())
                    });
                    let mut steps = 0;
                    while !handle.is_finished() {
                        if stop_after == Some(steps) {
                            handle.abort();
                            break;
                        }
                        if sim.pending_event_count() > 0 {
                            sim.step();
                            steps += 1;
                        }
                        tokio::task::yield_now().await;
                    }
                    let _ = handle.await;
                    (sim, steps)
                }
            };
            let (_, steps) = run_until(None).await;
            let (mut sim, _) = run_until(Some(steps - 1)).await;
            sim.simulate_crash_for_process(ip(), true);
            let config2 = config.clone();
            let (journal, recovery) = run(&mut sim, |provider| async move {
                Journal::open(provider, DIR, config2).await
            })
            .await
            .expect("reopen");
            assert_eq!(recovery.checkpoint, Some(3..5), "{config:?}");
            assert_eq!(
                journal.next_index(),
                6,
                "{config:?}: the cut checkpoint is gone"
            );
        }
    });
}

/// A checkpoint's first entry records how many entries it has, so one cut
/// short — here by a suffix truncation through it — is never named,
/// though every entry left of it is intact.
#[test]
fn a_checkpoint_cut_short_is_never_named() {
    runtime().block_on(async {
        let mut sim = sim(52);
        run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider, small()).await?;
            append_each(&mut journal, 7, &[64; 2]).await?;
            let c1 = checkpoint_of(&mut journal, 2).await?;
            let c2 = checkpoint_of(&mut journal, 5).await?;
            journal.truncate_suffix(c2.start + 2).await?;
            assert_eq!(journal.checkpoint(), Some(c1));
            Ok::<_, JournalError>(())
        })
        .await
        .expect("write");
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(recovery.checkpoint, Some(3..5));
        assert_eq!(journal.next_index(), 7, "the fragment stays, unnamed");
    });
}

/// A checkpoint is one batch and one sync: when the current segment cannot
/// hold it, the journal rolls over first rather than split it; one that no
/// segment could hold is refused before anything is written.
#[test]
fn a_checkpoint_is_never_split_across_segments() {
    runtime().block_on(async {
        let mut sim = sim(53);
        run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider.clone(), small()).await?;
            // 120 of the 128 slots: a 20-entry checkpoint no longer fits.
            let next = journal.next_index();
            let bytes: Vec<Vec<u8>> = (next..next + 120).map(|index| payload(index, 16)).collect();
            let records: Vec<Record<'_>> = bytes
                .iter()
                .zip(next..)
                .map(|(payload, index)| Record::new(7, payload).with_tag(tag(index, 7)))
                .collect();
            journal.append(&records).await?;
            let c = checkpoint_of(&mut journal, 20).await?;
            assert_eq!(c, 121..141);
            assert!(
                provider.exists(&segment_path(121)).await?,
                "rolled over first"
            );
            assert_eq!(journal.checkpoint(), Some(c));
            let too_large = checkpoint_of(&mut journal, 129).await;
            assert!(
                matches!(
                    too_large,
                    Err(JournalError::CheckpointTooLarge { entries: 129, .. })
                ),
                "{too_large:?}"
            );
            assert_eq!(journal.next_index(), 141, "nothing written");
            Ok::<_, JournalError>(())
        })
        .await
        .expect("write");
    });
}

/// EIO is zero-filled into a checksum mismatch, as CLSTORE does: the entries
/// in the unreadable block are corrupt, not an error that stops the journal.
#[test]
fn an_unreadable_entry_is_reported_corrupt() {
    runtime().block_on(async {
        let mut sim = sim(12);
        run(&mut sim, |provider| async move {
            let (mut journal, _) = open(provider, small()).await?;
            let bytes: Vec<Vec<u8>> = (1..=5).map(|index| payload(index, 4000)).collect();
            let records: Vec<Record<'_>> = bytes
                .iter()
                .zip(1..)
                .map(|(payload, index)| Record::new(7, payload).with_tag(tag(index, 7)))
                .collect();
            journal.append(&records).await?;
            append_each(&mut journal, 7, &[64]).await
        })
        .await
        .expect("write");
        // One batch of five, packed: entry i starts at 16384 + (i - 1) ×
        // 4064, so block 6 (24576..28672) holds the tail of entry 3 and the
        // head of entry 4. A later batch makes the damage corruption.
        sim.fail_file_with_eio(&segment_path(1), 50..51, EioTarget::Read)
            .expect("segment exists");
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        let indexes: Vec<u64> = recovery.corrupt.iter().map(|id| id.index).collect();
        assert_eq!(indexes, vec![3, 4]);
        assert_eq!(journal.next_index(), 7);
    });
}

#[test]
fn a_damaged_segment_header_is_repaired_from_its_twin() {
    runtime().block_on(async {
        let mut sim = sim(13);
        five_entries(&mut sim).await;
        flip(&mut sim, segment_path(1), 8).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(recovery.headers_repaired, 1);
        assert_eq!(journal.next_index(), 6);
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

    fn pick(&mut self, choices: &[f64]) -> f64 {
        choices[usize::try_from(self.below(choices.len() as u64)).expect("small")]
    }
}

/// Which crash physics a seed runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Model {
    /// The paper's: every unsynced sector ends up old or new (sector-atomic
    /// writes, arbitrarily reordered), plus clean crashes and correlated
    /// rollbacks. Nothing acknowledged may be lost or reported corrupt.
    Paper,
    /// Moonpool's full physics on top: sectors lost to garbage, latent read
    /// faults, shorn sectors. These can destroy bytes a sync already made
    /// durable when their sector is rewritten, which the paper's model rules
    /// out. The journal never rewrites a sector holding an acknowledged
    /// entry, so the guarantee is the paper's all the same; only
    /// acknowledged identifiers can be lost, and they are rebuilt.
    Harsh,
    /// The full physics again, aimed: after every write the writer hands
    /// the journal's [`JournalAtlas`] to the disk as a [`FaultFocus`], so the
    /// crash damages identifiers and live entries far more often than the
    /// zeros around them. Same guarantee as `Harsh`.
    Aimed,
}

impl Model {
    /// Whether this model's disk can destroy bytes a sync made durable.
    fn harsh(self) -> bool {
        self != Model::Paper
    }
}

/// The disk weights an aimed writer installs: identifiers heaviest, then
/// entries and the twin copies; the preallocated zeros barely at all.
fn focus_of(atlas: &JournalAtlas) -> FaultFocus {
    FaultFocus::new()
        .background(0.1)
        .layout(atlas.layout(), |region| {
            if region.kind == JournalRegion::SLOT {
                8.0
            } else {
                4.0
            }
        })
}

fn storage(model: Model, rng: &mut Rng) -> StorageConfiguration {
    let mut config = StorageConfiguration::fast_local();
    config.clean_crash_probability = rng.pick(&[0.0, 0.1]);
    config.correlated_rollback_probability = rng.pick(&[0.0, 0.2]);
    config.garbage_fill_probability = rng.pick(&[0.0, 0.5, 1.0]);
    config.crash_lost_probability = 0.0;
    config.crash_latent_fault_probability = 0.0;
    config.shorn_write_probability = 0.0;
    // A failed sync is an operating error: the write it covered is not
    // acknowledged, and the writer retries a failed metadata save.
    config.sync_failure_probability = rng.pick(&[0.0, 0.02, 0.1]);
    // Every unsynced name change may be undone at a crash, each on its own:
    // a prefix truncation's unlinks among them.
    config.unsynced_dir_entry_loss_probability = rng.pick(&[0.0, 0.5, 1.0]);
    if model.harsh() {
        config.crash_lost_probability = rng.pick(&[0.02, 0.1, 0.3]);
        config.crash_latent_fault_probability = rng.pick(&[0.0, 0.02, 0.1]);
        config.shorn_write_probability = rng.pick(&[0.0, 0.05, 0.2]);
    }
    config
}

/// What the writer knows: `(index, epoch, payload)` for every acknowledged
/// entry still in the log, in order.
type Ledger = Arc<Mutex<Vec<(u64, u64, Vec<u8>)>>>;

/// What the writer knows of its metadata: the last value `save_meta`
/// acknowledged, and every value it tried to save since — failed saves it
/// retried with a fresh value, and the one in flight when the crash hit.
/// Any of them may be what a reopen finds.
#[derive(Debug, Default, Clone)]
struct MetaState {
    acked: Option<u64>,
    pending: Vec<u64>,
    /// Saves that failed and were retried, since the last recovery.
    retried: usize,
}

type MetaLedger = Arc<Mutex<MetaState>>;

/// Tallies across seeds, to show the crashes landed where they matter.
#[derive(Debug, Default)]
struct Tally {
    torn: usize,
    /// Recoveries whose last batch had damaged entries.
    ambiguous: usize,
    /// Damaged entries of last batches, all recoveries together.
    ambiguous_entries: usize,
    /// Recoveries whose last batch had more than one damaged entry — the
    /// case a last-entry-only rule would misreport as corruption.
    ambiguous_multi: usize,
    ambiguous_kept: usize,
    corrupt_unacked: usize,
    /// Identifiers rebuilt from their intact entries, all recoveries
    /// together.
    slots_rewritten: usize,
    meta_repaired: usize,
    /// Metadata saves that failed and were retried with a newer value.
    meta_retried: usize,
    /// Opens that failed on an I/O error (a failed sync) and were retried:
    /// an operating error, not a verdict on the data.
    open_retried: usize,
    /// Recoveries whose log no longer starts at index 1: a prefix
    /// truncation dropped segments.
    compacted: usize,
    /// Recoveries that found a checkpoint.
    checkpointed: usize,
    /// ... with the whole history still there to judge its replay against.
    checkpoint_judged: usize,
}

fn seeds(default: u64) -> std::ops::RangeInclusive<u64> {
    let env = |name: &str| std::env::var(name).ok().and_then(|s| s.parse().ok());
    if let Some(seed) = env("JOURNAL_CRASH_SEED") {
        return seed..=seed;
    }
    1..=env("JOURNAL_CRASH_SEEDS").unwrap_or(default)
}

#[test]
fn under_the_papers_fault_model_no_acknowledged_entry_is_lost_or_corrupt() {
    let mut tally = Tally::default();
    for seed in seeds(250) {
        crash_loop(seed, Model::Paper, &mut tally);
    }
    eprintln!("paper model: {tally:?}");
    assert!(tally.torn > 0, "no crash ever tore a tail");
    assert!(
        tally.ambiguous_multi > 0,
        "no crash ever tore several entries of one batch"
    );
    assert!(tally.ambiguous_kept > 0, "no ambiguous batch was kept");
    assert!(
        tally.meta_repaired > 0,
        "no crash ever split the metadata copies"
    );
    assert!(
        tally.meta_retried > 0,
        "no failed metadata save was ever retried"
    );
    assert!(
        tally.compacted > 0,
        "no prefix truncation ever dropped a segment"
    );
    assert!(
        tally.checkpoint_judged > 0,
        "no replay from a checkpoint was judged against the history"
    );
    assert_eq!(
        tally.corrupt_unacked, 0,
        "under the paper's model, damage is only ever the unsynced last batch"
    );
}

/// The full physics (crash-lost sectors up to 0.3, latent faults up to 0.1,
/// shorn writes up to 0.2, garbage fills) and the paper's promise all the
/// same: every acknowledged entry survives, none is reported corrupt or
/// ambiguous, and every boot opens. A torn last batch, entries and
/// identifiers alike, ends the log.
#[test]
fn under_moonpools_full_physics_no_acknowledged_entry_is_lost_or_corrupt() {
    let mut tally = Tally::default();
    for seed in seeds(250) {
        crash_loop(seed, Model::Harsh, &mut tally);
    }
    eprintln!("harsh model: {tally:?}");
    assert!(tally.torn > 0, "no crash ever tore a tail");
    assert!(
        tally.slots_rewritten > 0,
        "no crash ever cost an acknowledged slot"
    );
}

/// The same physics aimed by the atlas: the guarantee holds, and more of
/// the damage lands on identifiers. A crash can only damage the sectors
/// dirty at that moment — the batch being written, and the slot-table
/// blocks its slots share with acknowledged ones — so the focus re-weighs a
/// small set, and the acknowledged identifiers it costs are rebuilt from
/// their entries.
#[test]
fn aimed_by_the_atlas_the_physics_cost_more_identifiers() {
    let (mut uniform, mut aimed) = (Tally::default(), Tally::default());
    for seed in seeds(120) {
        crash_loop(seed, Model::Harsh, &mut uniform);
        crash_loop(seed, Model::Aimed, &mut aimed);
    }
    eprintln!("uniform: {uniform:?}\naimed: {aimed:?}");
    assert!(
        aimed.slots_rewritten > uniform.slots_rewritten,
        "aiming should raise the identifiers recovery rebuilds: {} vs {}",
        aimed.slots_rewritten,
        uniform.slots_rewritten
    );
}

/// The crash loop's geometry: 48 KiB segments (eight 4 KiB batches of
/// data), so rollover and prefix truncation — and the crashes between a
/// truncation's unlinks — come every few rounds.
fn looped() -> Geometry {
    Geometry {
        segment_size: 48 * 1024,
        ..small().geometry
    }
}

/// Crash a writer repeatedly, checking every recovery against the ledger.
/// Odd seeds keep the ambiguous last batch instead of truncating it.
fn crash_loop(seed: u64, model: Model, tally: &mut Tally) {
    runtime().block_on(async {
        let mut sim = SimWorld::new_with_seed(seed);
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        sim.set_storage_config(storage(model, &mut rng));
        let config = JournalConfig {
            geometry: looped(),
            ..if seed % 2 == 1 { keep() } else { small() }
        };
        // A third of the seeds checkpoint, and judge every replay from the
        // checkpoint against the history.
        let checkpointing = seed.is_multiple_of(3);
        let ledger: Ledger = Arc::new(Mutex::new(Vec::new()));
        let meta: MetaLedger = Arc::new(Mutex::new(MetaState::default()));

        for round in 0..6 {
            let at = format!("{model:?} seed {seed} round {round}");
            let check = Check {
                ledger: &ledger,
                meta: &meta,
                config: &config,
                checkpointing,
                at: &at,
            };
            if !recover_and_check(&mut sim, &check, tally).await {
                return;
            }
            let ops = 1 + rng.below(12);
            let plan: Vec<(u64, u64, u64)> = (0..ops)
                .map(|_| {
                    let len = if rng.below(2) == 0 {
                        16 + rng.below(200)
                    } else {
                        16 + rng.below(3000)
                    };
                    let kind = match rng.below(10) {
                        // A checkpointing writer drops its prefix only
                        // behind a checkpoint; any other appends there.
                        2 if checkpointing => 4,
                        3 if !checkpointing => 9,
                        kind => kind,
                    };
                    (kind, 1 + rng.below(9), len)
                })
                .collect();
            let handle = tokio::spawn(write(
                sim.storage_provider(ip()),
                model,
                config.clone(),
                round + 1,
                plan,
                Arc::clone(&ledger),
                Arc::clone(&meta),
            ));
            for _ in 0..rng.below(400) {
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
    });
}

/// What one recovery is judged against.
struct Check<'a> {
    ledger: &'a Ledger,
    meta: &'a MetaLedger,
    config: &'a JournalConfig,
    /// The writer checkpoints: judge every replay from the checkpoint.
    checkpointing: bool,
    at: &'a str,
}

/// Reopen and judge the recovery against the ledger, then play the
/// replication layer: an entry reported corrupt that was never acknowledged
/// was never committed, so the log is cut before the first unreadable entry.
/// Returns whether the seed can go on.
async fn recover_and_check(sim: &mut SimWorld, check: &Check<'_>, tally: &mut Tally) -> bool {
    let at = check.at;
    let acked = check.ledger.lock().expect("ledger").clone();
    let acked_end = acked.last().map_or(1, |(index, _, _)| index + 1);
    let mut outcome = Err(JournalError::Poisoned);
    for _ in 0..8 {
        outcome = recover_once(sim, check.config.clone()).await;
        if !matches!(outcome, Err(JournalError::Io(_))) {
            break;
        }
        // A sync failed under the recovery's repairs: an operating error,
        // and opening again is how a caller recovers from it.
        tally.open_retried += 1;
    }
    let (recovery, entries, meta, start) = match outcome {
        Ok(found) => found,
        // Nothing a crash does to the sectors being written may stop the
        // journal from opening, under any model: the damage is only ever
        // the unsynced last batch, and that ends the log.
        Err(error) => panic!("{at}: a crash must never stop the journal opening: {error}"),
    };
    judge_reports(check, &recovery, acked_end, tally);
    tally.compacted += usize::from(start > 1);
    if check.checkpointing {
        judge_checkpoint(check, &recovery, &entries, start, tally);
    }
    // Every acknowledged index is still there and reads back exactly what
    // was acknowledged — under the full physics too: no write rewrites a
    // sector holding an acknowledged entry, so no crash can reach one. A
    // prefix truncation drops its entries from the ledger before it starts,
    // so the log may start earlier than the ledger, never later.
    let first = entries.first().map_or(u64::MAX, |entry| entry.index);
    for (index, epoch, payload) in &acked {
        let found = index
            .checked_sub(first)
            .and_then(|at| entries.get(usize::try_from(at).ok()?));
        let Some(entry) = found else {
            panic!("{at}: acknowledged entry {index} lost (log holds {first}..)");
        };
        assert_eq!(
            (entry.index, entry.epoch, &entry.tag, &entry.payload),
            (*index, *epoch, &tag(*index, *epoch), payload),
            "{at}: a read returned something other than what was acknowledged"
        );
    }
    *check.ledger.lock().expect("ledger") = entries
        .into_iter()
        .map(|entry| (entry.index, entry.epoch, entry.payload))
        .collect();

    // The metadata is the last acknowledged value or the one being saved.
    let mut state = check.meta.lock().expect("meta ledger");
    let found = meta.map(|bytes| u64::from_le_bytes(bytes.try_into().expect("8-byte meta")));
    assert!(
        found == state.acked || found.is_some_and(|value| state.pending.contains(&value)),
        "{at}: metadata {found:?} is neither acknowledged nor in flight: {state:?}"
    );
    tally.meta_retried += state.retried;
    *state = MetaState {
        acked: found,
        ..MetaState::default()
    };
    true
}

/// What one reopen found: the recovery report, the entries that read back
/// (cut before the first unreadable one, as a replication layer would cut
/// an uncommitted suffix), and the metadata.
type Recovered = (Recovery, Vec<Entry>, Option<Vec<u8>>, u64);

/// Reopen once and read everything back. Every entry is read twice — one
/// `read` per index and one `read_range` — and the two must agree.
async fn recover_once(
    sim: &mut SimWorld,
    config: JournalConfig,
) -> Result<Recovered, JournalError> {
    run(sim, |provider| async move {
        let (mut journal, recovery) = open(provider, config).await?;
        let (start, next) = (journal.start_index(), journal.next_index());
        let batched = journal.read_range(start..next).await?;
        let mut entries = Vec::new();
        for (index, many) in (start..next).zip(&batched) {
            let one = journal.read(index).await;
            match (&one, many) {
                (Ok(one), Ok(many)) => assert_eq!(one, many, "read and read_range disagree"),
                (Err(JournalError::Corrupt(one)), Err(many)) => assert_eq!(one, many),
                (one, many) => panic!("index {index}: read {one:?}, read_range {many:?}"),
            }
            entries.push(one);
        }
        if let Some(bad) = entries.iter().position(Result::is_err) {
            journal.truncate_suffix(start + bad as u64).await?;
            entries.truncate(bad);
        }
        let entries: Vec<Entry> = entries
            .into_iter()
            .map(|entry| entry.expect("kept"))
            .collect();
        Ok((recovery, entries, journal.meta().map(<[u8]>::to_vec), start))
    })
    .await
}

/// A replay from the recovered checkpoint gives the state the whole history
/// gives, whenever the history is still there; once the prefix is gone, a
/// checkpoint stands in for it. Any image records after the checkpoint are
/// a later one's remains, cut by a crash or by the replication layer, and a
/// replay skips them.
fn judge_checkpoint(
    check: &Check<'_>,
    recovery: &Recovery,
    entries: &[Entry],
    start: u64,
    tally: &mut Tally,
) {
    let at = check.at;
    let Some(c) = &recovery.checkpoint else {
        assert_eq!(
            start, 1,
            "{at}: the prefix is gone and no checkpoint stands in for it"
        );
        return;
    };
    let from = usize::try_from(c.start - start).expect("small");
    let len = usize::try_from(c.end - c.start).expect("small");
    assert!(
        from + len <= entries.len(),
        "{at}: checkpoint {c:?} is not readable (log {start}..+{})",
        entries.len()
    );
    assert!(
        entries[from..from + len]
            .iter()
            .all(|entry| entry.epoch & IMAGE != 0),
        "{at}: checkpoint {c:?} holds a record that is not an image"
    );
    tally.checkpointed += 1;
    if start == 1 {
        assert_eq!(
            replay(&entries[from..], len),
            history(entries),
            "{at}: replaying from checkpoint {c:?} differs from replaying the history"
        );
        tally.checkpoint_judged += 1;
    }
}

/// Judge what the recovery reported against the ledger, and tally it.
fn judge_reports(check: &Check<'_>, recovery: &Recovery, acked_end: u64, tally: &mut Tally) {
    let at = check.at;
    tally.torn += usize::from(recovery.torn_tail);
    let ambiguous = !recovery.ambiguous_batch.is_empty();
    tally.ambiguous += usize::from(ambiguous);
    tally.ambiguous_entries += recovery.ambiguous_batch.len();
    tally.ambiguous_multi += usize::from(recovery.ambiguous_batch.len() > 1);
    tally.meta_repaired += usize::from(recovery.meta_repaired);
    tally.slots_rewritten += recovery.slots_rewritten;
    if check.config.ambiguous_tail == AmbiguousTail::Keep {
        tally.ambiguous_kept += usize::from(ambiguous);
    }
    for id in recovery.corrupt.iter().chain(&recovery.ambiguous_batch) {
        assert_eq!(
            id.tag,
            tag(id.index, id.epoch),
            "{at}: a corrupt entry is reported with the tag it was written with"
        );
    }
    for id in &recovery.ambiguous_batch {
        // Ambiguous is not a license to lose data: only a batch whose sync
        // never returned can be damaged.
        assert!(
            id.index >= acked_end,
            "{at}: acknowledged entry {} reported ambiguous: {recovery:?}",
            id.index
        );
    }
    for id in &recovery.corrupt {
        assert!(
            id.index >= acked_end,
            "{at}: acknowledged entry {} reported corrupt: {recovery:?}",
            id.index
        );
        tally.corrupt_unacked += 1;
    }
}

/// Save `value` as the metadata. A failed save is retried with a newer
/// value, as a caller whose promise moved on would: the retry must win over
/// what the failure left behind.
async fn save_meta_retrying(
    journal: &mut Journal<SimStorageProvider>,
    meta: &MetaLedger,
    value: u64,
) -> Result<(), JournalError> {
    for value in value..value + 3 {
        meta.lock().expect("meta ledger").pending.push(value);
        if journal.save_meta(&value.to_le_bytes()).await.is_ok() {
            let mut state = meta.lock().expect("meta ledger");
            state.acked = Some(value);
            state.pending.clear();
            return Ok(());
        }
        meta.lock().expect("meta ledger").retried += 1;
    }
    Err(JournalError::Poisoned)
}

/// The epoch bit of a checkpoint image record in the crash loop.
const IMAGE: u64 = 1 << 62;

/// State digests per image record: few, so a checkpoint spans several
/// slot sectors and a crash can cut it short.
const IMAGE_CHUNK: usize = 8;

/// What the crash loop's writer folds its log into: the digest of every
/// ordinary record, in order — here, from the records themselves.
fn history<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Vec<u32> {
    entries
        .into_iter()
        .filter(|entry| entry.epoch & IMAGE == 0)
        .map(|entry| crc32c::crc32c(&entry.payload))
        .collect()
}

/// The same state replayed from a checkpoint, as a caller does: the image
/// its first `len` entries hold, then every ordinary record after it.
fn replay(entries: &[Entry], len: usize) -> Vec<u32> {
    let (image, rest) = entries.split_at(len);
    let mut state: Vec<u32> = image
        .iter()
        .flat_map(|entry| {
            entry.payload[4..]
                .chunks_exact(4)
                .map(|digest| u32::from_le_bytes(digest.try_into().expect("4 bytes")))
        })
        .collect();
    state.extend(history(rest));
    state
}

/// The image of `state`: its digests, `IMAGE_CHUNK` per record, each record
/// led by its position. An empty state is one empty record.
fn image_of(state: &[u32]) -> Vec<Vec<u8>> {
    if state.is_empty() {
        return vec![0u32.to_le_bytes().to_vec()];
    }
    state
        .chunks(IMAGE_CHUNK)
        .enumerate()
        .map(|(at, chunk)| {
            let position = u32::try_from(at * IMAGE_CHUNK).expect("small");
            let mut bytes = position.to_le_bytes().to_vec();
            for digest in chunk {
                bytes.extend(digest.to_le_bytes());
            }
            bytes
        })
        .collect()
}

/// The writer's state as its open finds it: replayed from the newest
/// checkpoint, or the history from the start when there is none.
async fn folded(journal: &Journal<SimStorageProvider>) -> Result<Vec<u32>, JournalError> {
    let next = journal.next_index();
    let checkpoint = journal.checkpoint();
    let from = checkpoint
        .as_ref()
        .map_or(journal.start_index(), |c| c.start);
    let entries = journal
        .read_range(from..next)
        .await?
        .into_iter()
        .map(|entry| entry.map_err(JournalError::Corrupt))
        .collect::<Result<Vec<Entry>, _>>()?;
    Ok(match checkpoint {
        Some(c) => replay(&entries, usize::try_from(c.end - c.start).expect("small")),
        None => history(&entries),
    })
}

/// The writer: `(kind, count, len)` steps of appends and, one time in ten
/// each, a suffix truncation at an arbitrary index, a metadata save, or a
/// prefix truncation. A checkpointing writer folds its records into a state
/// and, one time in ten, checkpoints it (kind 3); its prefix truncations
/// drop only what its newest checkpoint supersedes (kind 4), and its suffix
/// truncations never cut into that checkpoint.
async fn write(
    provider: SimStorageProvider,
    model: Model,
    config: JournalConfig,
    epoch: u64,
    plan: Vec<(u64, u64, u64)>,
    ledger: Ledger,
    meta: MetaLedger,
) -> Result<(), JournalError> {
    let disk = provider.clone();
    let (mut journal, _) = open(provider, config).await?;
    let aim = |journal: &Journal<SimStorageProvider>| {
        if model == Model::Aimed {
            disk.focus_faults(focus_of(&journal.atlas()))
                .expect("simulation alive");
        }
    };
    aim(&journal);
    let mut state = folded(&journal).await?;
    for (kind, count, len) in plan {
        let next = journal.next_index();
        let start = journal.start_index();
        // A checkpoint is never cut into: a fragment of one is no image.
        let floor = journal
            .checkpoint()
            .map_or(start + 1, |c| c.end.max(start + 1));
        if kind == 0 && next > floor {
            let from = floor + len % (next - floor);
            // The dropped entries stop being guaranteed the moment the
            // truncation starts.
            ledger
                .lock()
                .expect("ledger")
                .retain(|(index, _, _)| *index < from);
            journal.truncate_suffix(from).await?;
            state = folded(&journal).await?;
            aim(&journal);
            continue;
        }
        if kind == 3 {
            // A checkpoint: the state re-emitted as one batch.
            let image = image_of(&state);
            let epoch = epoch | IMAGE;
            let records: Vec<Record<'_>> = image
                .iter()
                .zip(next..)
                .map(|(payload, index)| Record::new(epoch, payload).with_tag(tag(index, epoch)))
                .collect();
            let range = journal.append_checkpoint(&records).await?;
            aim(&journal);
            let mut ledger = ledger.lock().expect("ledger");
            for (index, payload) in range.zip(image) {
                ledger.push((index, epoch, payload));
            }
            continue;
        }
        if kind == 4 {
            // Drop what the newest checkpoint supersedes, and only that.
            if let Some(c) = journal.checkpoint() {
                ledger
                    .lock()
                    .expect("ledger")
                    .retain(|(index, _, _)| *index >= c.start);
                journal.truncate_prefix(c.start).await?;
                aim(&journal);
            }
            continue;
        }
        if kind == 2 {
            // Compaction. The dropped entries stop being guaranteed the
            // moment it starts; the journal keeps the segment holding
            // `before`, so what it keeps below `before` is not lost either.
            let before = start + len % (next - start + 1);
            ledger
                .lock()
                .expect("ledger")
                .retain(|(index, _, _)| *index >= before);
            journal.truncate_prefix(before).await?;
            aim(&journal);
            continue;
        }
        if kind == 1 {
            save_meta_retrying(&mut journal, &meta, epoch * 1000 + len * 4).await?;
            aim(&journal);
            continue;
        }
        let len = usize::try_from(len).expect("small");
        let bytes: Vec<Vec<u8>> = (next..next + count)
            .map(|index| payload(index, len))
            .collect();
        let records: Vec<Record<'_>> = bytes
            .iter()
            .zip(next..)
            .map(|(payload, index)| Record::new(epoch, payload).with_tag(tag(index, epoch)))
            .collect();
        let range = journal.append(&records).await?;
        aim(&journal);
        state.extend(bytes.iter().map(|payload| crc32c::crc32c(payload)));
        let mut ledger = ledger.lock().expect("ledger");
        for (index, payload) in range.zip(bytes) {
            ledger.push((index, epoch, payload));
        }
    }
    Ok(())
}
