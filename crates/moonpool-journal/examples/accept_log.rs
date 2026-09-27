//! A Paxos acceptor's accept log on the real filesystem, through
//! `TokioStorageProvider` — the same journal the simulation example crashes,
//! run the way production would run it.
//!
//! 1. **Write.** The acceptor journals its accepts as a write-ahead log:
//!    each record is appended under the journal's own sequence number and
//!    tagged with the Paxos slot and ballot it is for. Its promise goes in
//!    the journal's two-copy metadata.
//! 2. **Restart.** Reopening runs the recovery scan; the acceptor replays
//!    the log with one `read_range` and rebuilds its state.
//! 3. **Rot.** One acknowledged entry in the middle of the log is damaged on
//!    disk, behind the journal's back. Reopening does not truncate there: it
//!    reports the entry corrupt, and the slot — stored far from the entry —
//!    still names the Paxos slot and ballot, so the acceptor knows exactly
//!    what to fetch again from a peer.
//! 4. **Repair.** The acceptor cuts the log at the corrupt entry, re-appends
//!    the peer's copy and the intact entries after it, and a final reopen
//!    comes back clean.
//!
//! Run with `cargo run -p moonpool-journal --example accept_log`.

use std::error::Error;
use std::path::PathBuf;

use moonpool_core::TokioStorageProvider;
use moonpool_journal::{Entry, EntryId, Geometry, Journal, JournalConfig, Record, TAG_SIZE, Tag};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// One accept: the acceptor voted for `value` in `slot` at `ballot`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Accept {
    slot: u64,
    ballot: u64,
    value: String,
}

impl Accept {
    fn new(slot: u64, ballot: u64, value: &str) -> Self {
        Self {
            slot,
            ballot,
            value: value.to_string(),
        }
    }

    /// The payload the journal stores. Unique per accept, so step 3 can find
    /// it on disk.
    fn payload(&self) -> Vec<u8> {
        format!(
            "accept slot={} ballot={} value={}",
            self.slot, self.ballot, self.value
        )
        .into_bytes()
    }

    /// The Paxos identity, kept in the journal's slot beside the index.
    fn tag(&self) -> Tag {
        encode_tag(self.slot, self.ballot)
    }

    fn from_entry(entry: &Entry) -> Result<Self> {
        let (slot, ballot) = decode_tag(&entry.tag);
        let text = std::str::from_utf8(&entry.payload)?;
        let value = text
            .rsplit_once("value=")
            .map(|(_, value)| value.to_string())
            .ok_or("payload without a value")?;
        Ok(Self {
            slot,
            ballot,
            value,
        })
    }
}

fn encode_tag(slot: u64, ballot: u64) -> Tag {
    let mut tag = [0; TAG_SIZE];
    tag[..8].copy_from_slice(&slot.to_le_bytes());
    tag[8..16].copy_from_slice(&ballot.to_le_bytes());
    tag
}

fn decode_tag(tag: &Tag) -> (u64, u64) {
    let word = |at: usize| {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&tag[at..at + 8]);
        u64::from_le_bytes(bytes)
    };
    (word(0), word(8))
}

/// A small geometry — 256 slots, 256 KiB segments — so the example does not
/// preallocate the default 64 MiB.
fn config() -> JournalConfig {
    JournalConfig {
        geometry: Geometry {
            slot_count: 256,
            data_start: 32 * 1024,
            segment_size: 256 * 1024,
        },
        ..JournalConfig::default()
    }
}

/// The acceptor's state as the journal rebuilds it.
struct Acceptor {
    journal: Journal<TokioStorageProvider>,
    accepts: Vec<Accept>,
}

impl Acceptor {
    /// Open (or create) the journal under `dir` and replay it.
    async fn open(dir: &str) -> Result<(Self, Vec<EntryId>)> {
        let (journal, recovery) = Journal::open(TokioStorageProvider::new(), dir, config()).await?;
        println!(
            "  opened: created={} entries=[{}, {}) torn_tail={} corrupt={}",
            recovery.created,
            journal.start_index(),
            journal.next_index(),
            recovery.torn_tail,
            recovery.corrupt.len(),
        );
        if let Some(meta) = journal.meta() {
            println!("  promise: {}", String::from_utf8_lossy(meta));
        }

        // Replay: one call, large sequential reads; a corrupt entry comes
        // back in place as its identity instead of ending the replay.
        let mut accepts = Vec::new();
        let mut damaged = Vec::new();
        let range = journal.start_index()..journal.next_index();
        for entry in journal.read_range(range).await? {
            match entry {
                Ok(entry) => accepts.push(Accept::from_entry(&entry)?),
                Err(id) => damaged.push(id),
            }
        }
        Ok((Self { journal, accepts }, damaged))
    }

    /// Journal a batch of accepts; on return every one is durable.
    async fn accept(&mut self, batch: &[Accept]) -> Result<()> {
        let payloads: Vec<Vec<u8>> = batch.iter().map(Accept::payload).collect();
        let records: Vec<Record<'_>> = batch
            .iter()
            .zip(&payloads)
            .map(|(accept, payload)| Record::new(accept.ballot, payload).with_tag(accept.tag()))
            .collect();
        let indexes = self.journal.append(&records).await?;
        println!("  appended {} accepts at indexes {indexes:?}", batch.len());
        self.accepts.extend_from_slice(batch);
        Ok(())
    }

    /// Durably promise not to accept below `ballot`.
    async fn promise(&mut self, ballot: u64) -> Result<()> {
        self.journal
            .save_meta(format!("promised ballot {ballot}").as_bytes())
            .await?;
        Ok(())
    }
}

/// Flip one byte of `needle` inside a segment file under `dir`: an
/// acknowledged entry rotting on disk.
fn rot(dir: &str, needle: &[u8]) -> Result<()> {
    for file in std::fs::read_dir(dir)? {
        let path = file?.path();
        let is_segment = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("seg-"));
        if !is_segment {
            continue;
        }
        let mut bytes = std::fs::read(&path)?;
        if let Some(at) = bytes.windows(needle.len()).position(|w| w == needle) {
            bytes[at + needle.len() - 1] ^= 0xff;
            std::fs::write(&path, bytes)?;
            return Ok(());
        }
    }
    Err("payload not found on disk".into())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let root: PathBuf =
        std::env::temp_dir().join(format!("moonpool-journal-example-{}", std::process::id()));
    let dir = root.join("wal");
    let dir = dir.to_str().ok_or("temporary path is not UTF-8")?;
    let outcome = run(dir).await;
    std::fs::remove_dir_all(&root)?;
    outcome
}

async fn run(dir: &str) -> Result<()> {
    println!("journal in {dir}");

    println!("\n1. write");
    let (mut acceptor, _) = Acceptor::open(dir).await?;
    acceptor.promise(7).await?;
    acceptor
        .accept(&[
            Accept::new(1, 7, "set x=1"),
            Accept::new(2, 7, "set y=2"),
            Accept::new(3, 7, "set x=3"),
        ])
        .await?;
    acceptor
        .accept(&[Accept::new(4, 7, "del y"), Accept::new(5, 7, "set z=5")])
        .await?;
    let written = acceptor.accepts.clone();
    drop(acceptor);

    println!("\n2. restart");
    let (acceptor, damaged) = Acceptor::open(dir).await?;
    assert!(damaged.is_empty());
    assert_eq!(acceptor.accepts, written);
    println!("  replayed {} accepts, all intact", acceptor.accepts.len());
    drop(acceptor);

    println!("\n3. rot slot 2's accept on disk, then restart");
    rot(dir, &written[1].payload())?;
    let (mut acceptor, damaged) = Acceptor::open(dir).await?;
    let [id] = damaged[..] else {
        return Err(format!("expected one corrupt entry, got {damaged:?}").into());
    };
    let (slot, ballot) = decode_tag(&id.tag);
    println!(
        "  entry {} (epoch {}) is corrupt: its slot says Paxos slot {slot}, ballot {ballot}",
        id.index, id.epoch
    );
    println!("  -> not a torn write: nothing was truncated, fetch it from a peer");

    println!("\n4. repair from a peer");
    let from_peer = Accept::new(slot, ballot, "set y=2");
    let after: Vec<Accept> = acceptor
        .accepts
        .iter()
        .filter(|accept| accept.slot > slot)
        .cloned()
        .collect();
    acceptor.journal.truncate_suffix(id.index).await?;
    acceptor.accepts.retain(|accept| accept.slot < slot);
    let mut batch = vec![from_peer];
    batch.extend(after);
    acceptor.accept(&batch).await?;
    drop(acceptor);

    let (acceptor, damaged) = Acceptor::open(dir).await?;
    assert!(damaged.is_empty());
    assert_eq!(acceptor.accepts, written);
    println!(
        "  replayed {} accepts, all intact again",
        acceptor.accepts.len()
    );
    Ok(())
}
