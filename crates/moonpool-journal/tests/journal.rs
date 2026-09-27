//! The journal over the simulator's storage: round trips, rollover,
//! truncation, tags, batched reads, the ambiguous-tail policy, metadata
//! repair, every row of the recovery table, and randomized crashes under two
//! fault models.

use std::future::Future;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_journal::{
    AmbiguousTail, Entry, EntryId, Geometry, Journal, JournalConfig, JournalError, Record,
    Recovery, TAG_SIZE, Tag,
};
use moonpool_sim::{EioTarget, SimStorageProvider, SimWorld, StorageConfiguration};

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
        // Entry 1 (10 bytes) takes 80 bytes; entry 2's payload starts 64
        // bytes into it.
        flip(&mut sim, segment_path(1), 16 * 1024 + 80 + 70).await;

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
        flip(&mut sim, segment_path(1), payload_byte(5)).await;
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
            assert_eq!(recovery.ambiguous_tail, Some(ambiguous));
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
        assert_eq!(recovery.corrupt, vec![ambiguous]);
        assert_eq!(recovery.ambiguous_tail, None);

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

/// Five single-entry batches of 64-byte payloads: each entry is 128 bytes
/// (a 64-byte header), packed contiguously from `data_start`.
fn entry_offset(index: u64) -> u64 {
    16 * 1024 + (index - 1) * 128
}

/// A byte inside entry `index`'s payload.
fn payload_byte(index: u64) -> u64 {
    entry_offset(index) + 80
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
        flip(&mut sim, segment_path(1), payload_byte(3)).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        let damaged = EntryId {
            index: 3,
            epoch: 7,
            tag: tag(3, 7),
        };
        assert_eq!(recovery.corrupt, vec![damaged], "reported with its tag");
        assert_eq!(recovery.ambiguous_tail, None);
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
        flip(&mut sim, segment_path(1), slot_offset(3) + 9).await;
        flip(&mut sim, segment_path(1), payload_byte(3)).await;
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
        zero(&mut sim, segment_path(1), slot_offset(4), 128).await;
        flip(&mut sim, segment_path(1), payload_byte(4)).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(
            journal.next_index(),
            4,
            "entry 4 and everything after it go"
        );
        assert!(recovery.torn_tail);
        assert_eq!(recovery.ambiguous_tail, None);
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
        flip(&mut sim, segment_path(1), payload_byte(5)).await;
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        assert_eq!(
            recovery.ambiguous_tail,
            Some(EntryId {
                index: 5,
                epoch: 7,
                tag: tag(5, 7)
            })
        );
        assert!(recovery.corrupt.is_empty());
        assert_eq!(journal.next_index(), 5);
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
            append_each(&mut journal, 7, &[4000; 5]).await
        })
        .await
        .expect("write");
        // Entry i starts at 16384 + (i - 1) × 4064; block 6 (24576..28672)
        // holds the tail of entry 3 and the head of entry 4.
        sim.fail_file_with_eio(&segment_path(1), 50..51, EioTarget::Read)
            .expect("segment exists");
        let (journal, recovery) = reopen(&mut sim).await.expect("reopen");
        let indexes: Vec<u64> = recovery.corrupt.iter().map(|id| id.index).collect();
        assert_eq!(indexes, vec![3, 4]);
        assert_eq!(journal.next_index(), 6);
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
    /// out — so the guarantee is detection: a read never returns wrong data.
    Harsh,
}

fn storage(model: Model, rng: &mut Rng) -> StorageConfiguration {
    let mut config = StorageConfiguration::fast_local();
    config.clean_crash_probability = rng.pick(&[0.0, 0.1]);
    config.correlated_rollback_probability = rng.pick(&[0.0, 0.2]);
    config.garbage_fill_probability = rng.pick(&[0.0, 0.5, 1.0]);
    config.crash_lost_probability = 0.0;
    config.crash_latent_fault_probability = 0.0;
    config.shorn_write_probability = 0.0;
    if model == Model::Harsh {
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
/// acknowledged, and one it was saving when the crash hit (either may be
/// what a reopen finds).
#[derive(Debug, Default, Clone, Copy)]
struct MetaState {
    acked: Option<u64>,
    pending: Option<u64>,
}

type MetaLedger = Arc<Mutex<MetaState>>;

/// Tallies across seeds, to show the crashes landed where they matter.
#[derive(Debug, Default)]
struct Tally {
    torn: usize,
    ambiguous: usize,
    ambiguous_kept: usize,
    corrupt_unacked: usize,
    refused: usize,
    corrupt_acked: usize,
    meta_repaired: usize,
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
        tally.corrupt_unacked > 0,
        "no crash ever tore inside a batch"
    );
    assert!(tally.ambiguous_kept > 0, "no ambiguous last entry was kept");
    assert!(
        tally.meta_repaired > 0,
        "no crash ever split the metadata copies"
    );
    assert_eq!(tally.refused + tally.corrupt_acked, 0);
}

#[test]
fn under_moonpools_full_physics_a_read_never_returns_wrong_data() {
    let mut tally = Tally::default();
    for seed in seeds(250) {
        crash_loop(seed, Model::Harsh, &mut tally);
    }
    eprintln!("harsh model: {tally:?}");
    assert!(tally.torn > 0, "no crash ever tore a tail");
}

/// Crash a writer repeatedly, checking every recovery against the ledger.
/// Odd seeds keep the ambiguous last entry instead of truncating it.
fn crash_loop(seed: u64, model: Model, tally: &mut Tally) {
    runtime().block_on(async {
        let mut sim = SimWorld::new_with_seed(seed);
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        sim.set_storage_config(storage(model, &mut rng));
        let config = if seed % 2 == 1 { keep() } else { small() };
        let ledger: Ledger = Arc::new(Mutex::new(Vec::new()));
        let meta: MetaLedger = Arc::new(Mutex::new(MetaState::default()));

        for round in 0..6 {
            let at = format!("{model:?} seed {seed} round {round}");
            let check = Check {
                ledger: &ledger,
                meta: &meta,
                config: &config,
                model,
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
                    (rng.below(10), 1 + rng.below(9), len)
                })
                .collect();
            let handle = tokio::spawn(write(
                sim.storage_provider(ip()),
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
    model: Model,
    at: &'a str,
}

/// Reopen and judge the recovery against the ledger, then play the
/// replication layer: an entry reported corrupt that was never acknowledged
/// was never committed, so the log is cut before the first unreadable entry.
/// Every entry is read twice — one `read` per index and one `read_range` —
/// and the two must agree. Returns whether the seed can go on.
async fn recover_and_check(sim: &mut SimWorld, check: &Check<'_>, tally: &mut Tally) -> bool {
    let (model, at) = (check.model, check.at);
    let acked = check.ledger.lock().expect("ledger").clone();
    let acked_end = acked.last().map_or(1, |(index, _, _)| index + 1);
    let config = check.config.clone();
    let outcome = run(sim, |provider| async move {
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
        Ok::<_, JournalError>((recovery, entries, journal.meta().map(<[u8]>::to_vec)))
    })
    .await;
    let (recovery, entries, meta) = match outcome {
        Ok(found) => found,
        Err(error) => {
            assert!(
                model == Model::Harsh,
                "{at}: the paper's model must always recover, got {error}"
            );
            tally.refused += 1;
            return false;
        }
    };
    tally.torn += usize::from(recovery.torn_tail);
    tally.ambiguous += usize::from(recovery.ambiguous_tail.is_some());
    tally.meta_repaired += usize::from(recovery.meta_repaired);
    if check.config.ambiguous_tail == AmbiguousTail::Keep {
        tally.ambiguous_kept += usize::from(recovery.ambiguous_tail.is_some());
    }
    for id in recovery.corrupt.iter().chain(&recovery.ambiguous_tail) {
        assert_eq!(
            id.tag,
            tag(id.index, id.epoch),
            "{at}: a corrupt entry is reported with the tag it was written with"
        );
        if id.index >= acked_end {
            tally.corrupt_unacked += 1;
        } else {
            assert!(
                model == Model::Harsh,
                "{at}: acknowledged entry {} reported corrupt: {recovery:?}",
                id.index
            );
            tally.corrupt_acked += 1;
        }
    }
    // Whatever a read returned for an acknowledged index is exactly what was
    // acknowledged; under the paper's model, every acknowledged index is
    // still there.
    for (entry, (index, epoch, payload)) in entries.iter().zip(&acked) {
        assert_eq!(
            (entry.index, entry.epoch, &entry.tag, &entry.payload),
            (*index, *epoch, &tag(*index, *epoch), payload),
            "{at}: a read returned something other than what was acknowledged"
        );
    }
    if model == Model::Paper {
        assert!(
            entries.len() >= acked.len(),
            "{at}: lost acknowledged entries ({} < {})",
            entries.len(),
            acked.len()
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
        found == state.acked || (found.is_some() && found == state.pending),
        "{at}: metadata {found:?} is neither acknowledged nor in flight: {state:?}"
    );
    *state = MetaState {
        acked: found,
        pending: None,
    };
    true
}

/// The writer: `(kind, count, len)` steps of appends and, one time in ten
/// each, a suffix truncation at an arbitrary index or a metadata save.
async fn write(
    provider: SimStorageProvider,
    config: JournalConfig,
    epoch: u64,
    plan: Vec<(u64, u64, u64)>,
    ledger: Ledger,
    meta: MetaLedger,
) -> Result<(), JournalError> {
    let (mut journal, _) = open(provider, config).await?;
    for (kind, count, len) in plan {
        let next = journal.next_index();
        let start = journal.start_index();
        if kind == 0 && next > start + 1 {
            let from = start + 1 + len % (next - start - 1);
            // The dropped entries stop being guaranteed the moment the
            // truncation starts.
            ledger
                .lock()
                .expect("ledger")
                .retain(|(index, _, _)| *index < from);
            journal.truncate_suffix(from).await?;
            continue;
        }
        if kind == 1 {
            let value = epoch * 1000 + len;
            meta.lock().expect("meta ledger").pending = Some(value);
            journal.save_meta(&value.to_le_bytes()).await?;
            *meta.lock().expect("meta ledger") = MetaState {
                acked: Some(value),
                pending: None,
            };
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
        let mut ledger = ledger.lock().expect("ledger");
        for (index, payload) in range.zip(bytes) {
            ledger.push((index, epoch, payload));
        }
    }
    Ok(())
}
