//! The journal over `CompioStorageProvider` on the real filesystem: the same
//! journal code as the Tokio run in `tests/real_filesystem.rs`, on the local
//! traits alone.
//!
//! Run with `cargo test -p moonpool-journal --features compio-providers`.
//! It pins that committed bytes read back after a reopen, buffered and with
//! direct I/O (where the filesystem under the target directory offers it),
//! across rolls onto allocated segments and onto recycled ones, whose old
//! batches read as stale.

use moonpool_core::{CompioStorageProvider, DirectIo};
use moonpool_journal::{
    Batch, Durability, Geometry, ID_SIZE, Journal, JournalConfig, JournalId, State,
};

/// Commits per phase: enough that the small geometry rolls several times.
const ROUNDS: u8 = 12;

/// Entry lengths around and across block boundaries.
const LENS: [usize; 6] = [1, 4095, 4096, 4097, 12_000, 30_000];

/// A payload whose period (251, a prime) lines up with no block, so a
/// misplaced or stale sector cannot read back equal by accident.
fn payload(position: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|at| {
            u8::try_from((at + usize::try_from(position).expect("small")) % 251).expect("fits")
        })
        .collect()
}

fn lens(round: u8) -> &'static [usize] {
    &LENS[..=usize::from(round) % LENS.len()]
}

/// Commit `ROUNDS` batches from `position` on, the first one raising the
/// floor to `floor`; every `(position, len)` written.
async fn write(
    journal: &mut Journal<CompioStorageProvider>,
    mut position: u64,
    floor: u64,
) -> Result<Vec<(u64, usize)>, Box<dyn std::error::Error>> {
    let mut written = Vec::new();
    for round in 0..ROUNDS {
        let mut batch = Batch::new();
        if round == 0 && floor > 0 {
            batch.truncate_prefix(floor);
        }
        for &len in lens(round) {
            batch.put(position, [round; ID_SIZE], payload(position, len));
            written.push((position, len));
            position += 1;
        }
        batch.set_meta(vec![round; 7]);
        journal.commit(batch).await?;
    }
    Ok(written)
}

/// Every entry of `written` reads back, one by one and in a replay.
async fn check(
    journal: &Journal<CompioStorageProvider>,
    written: &[(u64, usize)],
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(journal.meta(), [ROUNDS - 1; 7], "{name}: the last metainfo");
    for &(position, len) in written {
        assert!(matches!(journal.state(position), State::Live { .. }));
        let entry = journal.read(position).await?;
        assert_eq!(entry.payload, payload(position, len), "{name}: {position}");
    }
    let replayed = journal.replay(..).await?;
    assert_eq!(replayed.len(), written.len(), "{name}");
    for ((at, entry), &(position, _)) in replayed.into_iter().zip(written) {
        assert_eq!(at, position, "{name}");
        assert_eq!(entry?.position, at, "{name}");
    }
    Ok(())
}

async fn run(dir: &str, config: JournalConfig) -> Result<(), Box<dyn std::error::Error>> {
    let provider = CompioStorageProvider::new();
    let id = JournalId(11);
    let mut journal = Journal::create(provider, dir, id, config, b"m0").await?;
    let first = write(&mut journal, 0, 0).await?;
    assert!(journal.rolls().created > 1, "{dir}: {:?}", journal.rolls());
    let (journal, recovery) = Journal::open(provider, dir, id, config)
        .await?
        .ok_or("the journal is gone")?;
    assert!(recovery.corrupt.is_empty(), "{dir}: {recovery:?}");
    check(&journal, &first, dir).await?;

    // Raise the floor past everything: the dead segments are recycled, and
    // the rolls of the next phase land on them.
    let mut journal = journal;
    let next = first.last().map_or(0, |&(position, _)| position + 1);
    let second = write(&mut journal, next, next).await?;
    assert!(
        journal.rolls().onto_spare > 0,
        "{dir}: no roll landed on a recycled segment: {:?}",
        journal.rolls()
    );
    drop(journal);
    let (journal, recovery) = Journal::open(provider, dir, id, config)
        .await?
        .ok_or("the journal is gone")?;
    assert!(recovery.corrupt.is_empty(), "{dir}: {recovery:?}");
    assert_eq!(recovery.torn, 0, "{dir}: {recovery:?}");
    check(&journal, &second, dir).await
}

#[test]
fn committed_bytes_read_back_after_a_reopen_on_compio() -> Result<(), Box<dyn std::error::Error>> {
    // Under the target directory, not `/tmp`, which can be tmpfs (no
    // direct I/O there).
    let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("journal-compio-fs-{}", std::process::id()));
    let runtime = compio::runtime::Runtime::new()?;
    eprintln!("compio driver: {:?}", runtime.driver_type());
    for (name, durability, direct_io) in [
        ("ordered-direct", Durability::Ordered, DirectIo::Optional),
        ("batched-direct", Durability::Batched, DirectIo::Optional),
        ("ordered-buffered", Durability::Ordered, DirectIo::Disabled),
    ] {
        let dir = root.join(name);
        let dir = dir.to_str().ok_or("path is not UTF-8")?;
        let config = JournalConfig {
            durability,
            geometry: Geometry::small(),
            direct_io,
        };
        runtime.block_on(run(dir, config))?;
    }
    std::fs::remove_dir_all(&root)?;
    Ok(())
}
