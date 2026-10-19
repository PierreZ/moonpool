//! The journal over `CompioStorageProvider`: its batches, headers and
//! metainfo go to the kernel in their own aligned buffers (the zero-copy
//! path), with direct I/O where the filesystem under the target directory
//! offers it.

use moonpool_compio::CompioStorageProvider;
use moonpool_core::DirectIo;
use moonpool_journal::{
    Batch, Durability, Geometry, ID_SIZE, Journal, JournalConfig, JournalId, State,
};

/// A payload whose period (251, a prime) lines up with no block.
fn payload(position: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|at| {
            u8::try_from((at + usize::try_from(position).expect("small")) % 251).expect("fits")
        })
        .collect()
}

#[compio::test]
async fn committed_entries_recover_after_a_reopen() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::TempDir::new_in(env!("CARGO_TARGET_TMPDIR"))?;
    let provider = CompioStorageProvider::new();
    for (name, durability) in [
        ("ordered", Durability::Ordered),
        ("batched", Durability::Batched),
    ] {
        let dir = root.path().join(name);
        let dir = dir.to_str().ok_or("path is not UTF-8")?;
        let config = JournalConfig {
            durability,
            geometry: Geometry::small(),
            direct_io: DirectIo::Optional,
        };
        let id = JournalId(7);
        let mut journal = Journal::create(provider, dir, id, config, b"m0").await?;
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

        let (journal, recovery) = Journal::open(provider, dir, id, config)
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
    }
    Ok(())
}
