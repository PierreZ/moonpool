//! The journal over `CompioStorageProvider` on the real filesystem, with its
//! spares prepared on `CompioTaskProvider`: the same journal code as the
//! Tokio run in `tests/real_filesystem.rs`, on the local traits alone.
//!
//! Run with `cargo test -p moonpool-journal --features compio-providers`.
//! It pins that committed bytes read back after a reopen, buffered and with
//! direct I/O (where the filesystem under the target directory offers it),
//! that rolls land on prepared spares, inline and in the background, and
//! that the next open deletes a spare left behind.

use std::time::{Duration, Instant};

use moonpool_core::{CompioStorageProvider, CompioTaskProvider, DirectIo, LocalTaskProvider};
use moonpool_journal::{
    Batch, Durability, Geometry, ID_SIZE, Journal, JournalConfig, JournalId, SpareSpawner, State,
};

/// Commits per run: enough that the small geometry rolls several times.
const ROUNDS: u8 = 12;

/// Entry lengths around and across block boundaries.
const LENS: [usize; 6] = [1, 4095, 4096, 4097, 12_000, 30_000];

/// How long a background run lets its spare task work between commits.
const SPARE_TIME: Duration = Duration::from_millis(50);

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

/// How the run prepares spares.
#[derive(Clone, Copy, Debug)]
enum Spares {
    /// `prepare_spare` awaited before every commit.
    Inline,
    /// `prepare_spares_local` on `CompioTaskProvider`.
    Background,
}

/// Commit `ROUNDS` batches; the next position.
async fn write<S: SpareSpawner<CompioStorageProvider>>(
    journal: &mut Journal<CompioStorageProvider, S>,
    spares: Spares,
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut position = 0_u64;
    for round in 0..ROUNDS {
        match spares {
            Spares::Inline => journal.prepare_spare().await?,
            Spares::Background => {
                // A roll never waits for a spare: give the task its time.
                let until = Instant::now() + SPARE_TIME;
                while Instant::now() < until {
                    CompioTaskProvider.yield_now().await;
                }
            }
        }
        let mut batch = Batch::new();
        for &len in lens(round) {
            batch.put(position, [round; ID_SIZE], payload(position, len));
            position += 1;
        }
        batch.set_meta(vec![round; 7]);
        journal.commit(batch).await?;
    }
    if matches!(spares, Spares::Inline) {
        // Left behind for the next open to delete.
        journal.prepare_spare().await?;
    }
    Ok(position)
}

/// Every committed entry reads back, one by one and in a replay.
async fn check<S: SpareSpawner<CompioStorageProvider>>(
    journal: &Journal<CompioStorageProvider, S>,
    position: u64,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(journal.meta(), [ROUNDS - 1; 7], "{name}: the last metainfo");
    let mut expected = 0_u64;
    for round in 0..ROUNDS {
        for &len in lens(round) {
            assert!(matches!(journal.state(expected), State::Live { .. }));
            let entry = journal.read(expected).await?;
            assert_eq!(entry.payload, payload(expected, len), "{name}: {expected}");
            expected += 1;
        }
    }
    assert_eq!(expected, position);
    let replayed = journal.replay(..).await?;
    assert_eq!(replayed.len(), usize::try_from(position)?);
    for (at, entry) in replayed {
        assert_eq!(entry?.position, at, "{name}");
    }
    Ok(())
}

async fn run(
    dir: &str,
    config: JournalConfig,
    spares: Spares,
) -> Result<(), Box<dyn std::error::Error>> {
    let provider = CompioStorageProvider::new();
    let id = JournalId(11);
    let journal = Journal::create(provider, dir, id, config, b"m0").await?;
    let (position, rolls) = match spares {
        Spares::Inline => {
            let mut journal = journal;
            (write(&mut journal, spares).await?, journal.rolls())
        }
        Spares::Background => {
            let mut journal = journal.prepare_spares_local(CompioTaskProvider);
            (write(&mut journal, spares).await?, journal.rolls())
        }
    };
    assert!(
        rolls.onto_spare > 0,
        "{dir}: no roll landed on a spare: {rolls:?}"
    );

    // Reopen, on spares again: recovery deletes the spare left behind and
    // keeps every committed byte.
    let (journal, recovery) = Journal::open(provider, dir, id, config)
        .await?
        .ok_or("the journal is gone")?;
    assert!(recovery.corrupt.is_empty(), "{dir}: {recovery:?}");
    if matches!(spares, Spares::Inline) {
        assert!(recovery.spares_removed > 0, "{dir}: {recovery:?}");
    }
    let journal = journal.prepare_spares_local(CompioTaskProvider);
    check(&journal, position, dir).await
}

#[test]
fn committed_bytes_read_back_after_a_reopen_on_compio() -> Result<(), Box<dyn std::error::Error>> {
    // Under the target directory, not `/tmp`, which can be tmpfs (no
    // direct I/O there).
    let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("journal-compio-fs-{}", std::process::id()));
    let runtime = compio::runtime::Runtime::new()?;
    eprintln!("compio driver: {:?}", runtime.driver_type());
    for (name, durability, direct_io, spares) in [
        (
            "ordered-direct-inline",
            Durability::Ordered,
            DirectIo::Optional,
            Spares::Inline,
        ),
        (
            "batched-direct-background",
            Durability::Batched,
            DirectIo::Optional,
            Spares::Background,
        ),
        (
            "ordered-buffered-background",
            Durability::Ordered,
            DirectIo::Disabled,
            Spares::Background,
        ),
    ] {
        let dir = root.join(name);
        let dir = dir.to_str().ok_or("path is not UTF-8")?;
        let config = JournalConfig {
            durability,
            geometry: Geometry::small(),
            direct_io,
        };
        runtime.block_on(run(dir, config, spares))?;
    }
    std::fs::remove_dir_all(&root)?;
    Ok(())
}
