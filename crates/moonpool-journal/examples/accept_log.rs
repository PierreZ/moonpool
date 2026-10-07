//! A Paxos acceptor's durable state on the real filesystem, through
//! `TokioStorageProvider`: the same journal the simulation example crashes,
//! run the way production would run it.
//!
//! 1. **Write.** The acceptor stores each accept at its Paxos slot (the
//!    journal's position) with the ballot as its identity, in whatever order
//!    accepts arrive: a lagging acceptor fills old slots while new ones come
//!    in, and a higher ballot re-accepts a slot in place. Its promise is the
//!    journal's metainfo.
//! 2. **Restart.** Reopening runs the recovery scan; the acceptor replays
//!    every slot and rebuilds its state.
//! 3. **Rot.** One acknowledged accept is damaged on disk, behind the
//!    journal's back. Reading it does not return wrong bytes and does not
//!    truncate: it reports the slot damaged, with the ballot its persist
//!    record kept far from the entry, so the acceptor knows exactly what to
//!    fetch from a peer.
//! 4. **Repair.** The acceptor writes the peer's copy at the same slot, and
//!    a final reopen comes back clean.
//!
//! Run with `cargo run -p moonpool-journal --example accept_log`.

use std::error::Error;

use moonpool_core::TokioStorageProvider;
use moonpool_journal::{
    Batch, Durability, ID_SIZE, Id, Journal, JournalConfig, JournalId, ReadError,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const ACCEPTOR: JournalId = JournalId(0xACCE_9708);

fn ballot_id(ballot: u64) -> Id {
    let mut id = [0; ID_SIZE];
    id[..8].copy_from_slice(&ballot.to_le_bytes());
    id
}

fn ballot_of(id: &Id) -> u64 {
    u64::from_le_bytes(id[..8].try_into().expect("eight bytes"))
}

fn config() -> JournalConfig {
    JournalConfig {
        durability: Durability::Ordered,
        ..JournalConfig::default()
    }
}

async fn open(dir: &str) -> Result<Journal<TokioStorageProvider>> {
    let provider = TokioStorageProvider::new();
    match Journal::open(provider.clone(), dir, ACCEPTOR, config()).await? {
        Some((journal, recovery)) => {
            println!("  opened: {recovery:?}");
            Ok(journal)
        }
        None => Ok(Journal::create(provider, dir, ACCEPTOR, config(), b"promise=0").await?),
    }
}

/// Every slot and what it holds, as the acceptor rebuilds its state.
/// Returns the damaged slots with the ballots their records kept.
async fn replay(journal: &Journal<TokioStorageProvider>) -> Result<Vec<(u64, u64)>> {
    let mut damaged = Vec::new();
    for (slot, read) in journal.replay(..).await? {
        match read {
            Ok(entry) => println!(
                "  slot {slot}: ballot {} value {:?}",
                ballot_of(&entry.id),
                String::from_utf8_lossy(&entry.payload)
            ),
            Err(ReadError::Damaged { id, .. }) => {
                println!(
                    "  slot {slot}: DAMAGED, accepted at ballot {}: fetch it from a peer",
                    ballot_of(&id)
                );
                damaged.push((slot, ballot_of(&id)));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(damaged)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let root =
        std::env::temp_dir().join(format!("moonpool-journal-example-{}", std::process::id()));
    let dir = root.join("acceptor");
    let dir = dir
        .to_str()
        .ok_or("temporary path is not UTF-8")?
        .to_string();

    println!("1. write: accepts in arrival order, a promise in the metainfo");
    let mut journal = open(&dir).await?;
    let mut batch = Batch::new();
    batch
        .put(7, ballot_id(3), "set x=7")
        .put(9, ballot_id(3), "set y=9")
        .set_meta("promise=3");
    journal.commit(batch).await?;
    // A lagging acceptor catches up on older slots, and a new leader
    // re-accepts slot 9 at a higher ballot.
    let mut batch = Batch::new();
    batch
        .put(4, ballot_id(2), "set w=4")
        .put(9, ballot_id(5), "set y=10")
        .set_meta("promise=5");
    journal.commit(batch).await?;
    drop(journal);

    println!("2. restart and replay");
    let journal = open(&dir).await?;
    println!("  promise: {}", String::from_utf8_lossy(journal.meta()));
    assert_eq!(journal.meta(), b"promise=5");
    assert!(
        replay(&journal).await?.is_empty(),
        "every accept reads back"
    );
    let slots: Vec<u64> = journal.positions().map(|(slot, _)| slot).collect();
    assert_eq!(slots, [4, 7, 9]);

    println!("3. rot: slot 7's bytes are damaged on disk");
    let layout = journal.layout(7).ok_or("slot 7 is live")?;
    drop(journal);
    let mut bytes = std::fs::read(&layout.entry.path)?;
    let at = usize::try_from(layout.entry.bytes.end - 1)?;
    bytes[at] ^= 0xFF;
    std::fs::write(&layout.entry.path, bytes)?;
    let mut journal = open(&dir).await?;
    let damaged = replay(&journal).await?;
    assert_eq!(damaged, [(7, 3)], "slot 7 is damaged, its ballot known");

    println!("4. repair from a peer, then reopen");
    let mut batch = Batch::new();
    batch.put(7, ballot_id(3), "set x=7");
    journal.commit(batch).await?;
    drop(journal);
    let journal = open(&dir).await?;
    assert!(replay(&journal).await?.is_empty(), "repaired");

    std::fs::remove_dir_all(&root)?;
    Ok(())
}
