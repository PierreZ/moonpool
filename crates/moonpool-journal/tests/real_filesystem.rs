//! The journal over `TokioStorageProvider` on the real filesystem, where its
//! batches, headers and metainfo move to the kernel in their own aligned
//! buffers (the zero-copy path) rather than through a bounce buffer.
//!
//! The simulation proves the commit protocol; this pins only that the owned
//! transfers carry exactly the bytes the journal encoded, buffered and with
//! direct I/O (where the filesystem under the target directory offers it),
//! across segment rolls and a reopen.

use moonpool_core::{DirectIo, TokioStorageProvider};
use moonpool_journal::{
    Batch, Durability, Geometry, ID_SIZE, Journal, JournalConfig, JournalId, State,
};

/// A payload whose period (251, a prime) lines up with no block, so a
/// misplaced or stale sector cannot read back equal by accident.
fn payload(position: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|at| {
            u8::try_from((at + usize::try_from(position).expect("small")) % 251).expect("fits")
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_bytes_read_back_after_a_reopen() -> Result<(), Box<dyn std::error::Error>> {
    // Under the target directory, not `/tmp`, which can be tmpfs (no
    // direct I/O there).
    let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("journal-real-fs-{}", std::process::id()));
    let provider = TokioStorageProvider::new();
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
        let id = JournalId(11);
        let mut journal = Journal::create(provider.clone(), dir, id, config, b"m0").await?;
        // Lengths around and across block boundaries; enough commits that
        // the small geometry rolls onto new segments.
        let lens = [1, 4095, 4096, 4097, 12_000, 30_000];
        let mut position = 0_u64;
        for round in 0..6_u8 {
            let mut batch = Batch::new();
            for &len in &lens[..=usize::from(round) % lens.len()] {
                batch.put(position, [round; ID_SIZE], payload(position, len));
                position += 1;
            }
            batch.set_meta(vec![round; 7]);
            journal.commit(batch).await?;
        }
        drop(journal);

        let (journal, recovery) = Journal::open(provider.clone(), dir, id, config)
            .await?
            .ok_or("the journal is gone")?;
        assert!(recovery.corrupt.is_empty(), "{name}: {recovery:?}");
        assert_eq!(journal.meta(), [5_u8; 7], "{name}: the last metainfo");
        let mut expected = 0_u64;
        for round in 0..6_usize {
            for &len in &lens[..=round % lens.len()] {
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
    }
    std::fs::remove_dir_all(&root)?;
    Ok(())
}
