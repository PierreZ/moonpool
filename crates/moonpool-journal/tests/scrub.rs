//! `Journal::verify_records`, the read-only check of persist records that a
//! caller's scrub runs: it reads each persist block one time, in disk
//! order, and names only the positions whose record rotted.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};
use moonpool_core::{
    AlignedBuf, IoConstraints, LocalStorageFile, LocalStorageProvider, OpenOptions, StorageFile,
    StorageProvider,
};
use moonpool_journal::{
    Batch, Durability, Geometry, ID_SIZE, Id, Journal, JournalConfig, JournalId,
};
use moonpool_sim::{SimStorageProvider, SimWorld, StorageConfiguration};

const DIR: &str = "wal";
const JOURNAL: JournalId = JournalId(0x5C2B);

/// A provider that counts the read requests its files get: each block
/// read starts with one `read_at_aligned` (a short transfer is resumed
/// through `read_at`, which is not a new request).
#[derive(Clone)]
struct Counting<P> {
    inner: P,
    reads: Arc<AtomicU64>,
}

struct CountingFile<F> {
    inner: F,
    reads: Arc<AtomicU64>,
}

impl<P: LocalStorageProvider> LocalStorageProvider for Counting<P> {
    type File = CountingFile<P::File>;

    async fn open(&self, path: &str, options: OpenOptions) -> io::Result<Self::File> {
        Ok(CountingFile {
            inner: self.inner.open(path, options).await?,
            reads: Arc::clone(&self.reads),
        })
    }

    async fn exists(&self, path: &str) -> io::Result<bool> {
        self.inner.exists(path).await
    }

    async fn delete(&self, path: &str) -> io::Result<()> {
        self.inner.delete(path).await
    }

    async fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to).await
    }

    async fn create_dir_all(&self, path: &str) -> io::Result<()> {
        self.inner.create_dir_all(path).await
    }

    async fn list_dir(&self, path: &str) -> io::Result<Vec<String>> {
        self.inner.list_dir(path).await
    }

    async fn sync_dir(&self, path: &str) -> io::Result<()> {
        self.inner.sync_dir(path).await
    }
}

impl<F: LocalStorageFile> AsyncRead for CountingFile<F> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<F: LocalStorageFile> AsyncWrite for CountingFile<F> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

impl<F: LocalStorageFile> AsyncSeek for CountingFile<F> {
    fn poll_seek(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: io::SeekFrom,
    ) -> Poll<io::Result<u64>> {
        Pin::new(&mut self.inner).poll_seek(cx, pos)
    }
}

impl<F: LocalStorageFile> LocalStorageFile for CountingFile<F> {
    async fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all().await
    }

    async fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data().await
    }

    fn constraints(&self) -> IoConstraints {
        self.inner.constraints()
    }

    fn is_direct_io(&self) -> bool {
        self.inner.is_direct_io()
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read_at(offset, buf).await
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        self.inner.write_at(offset, buf).await
    }

    async fn write_at_durable(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        self.inner.write_at_durable(offset, buf).await
    }

    async fn read_at_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> io::Result<(usize, AlignedBuf)> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_at_aligned(offset, buf).await
    }

    async fn write_at_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> io::Result<(usize, AlignedBuf)> {
        self.inner.write_at_aligned(offset, buf).await
    }

    async fn write_at_durable_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> io::Result<(usize, AlignedBuf)> {
        self.inner.write_at_durable_aligned(offset, buf).await
    }

    async fn size(&self) -> io::Result<u64> {
        self.inner.size().await
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        self.inner.set_len(size).await
    }

    async fn allocate(&self, len: u64) -> io::Result<()> {
        self.inner.allocate(len).await
    }

    async fn drop_cache(&self) -> io::Result<()> {
        self.inner.drop_cache().await
    }
}

type J = Journal<Counting<SimStorageProvider>>;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
}

fn sim() -> SimWorld {
    let mut sim = SimWorld::new_with_seed(21);
    sim.set_storage_config(StorageConfiguration::fast_local());
    sim
}

/// Run `f` against `sim`'s storage, stepping the world until it finishes.
async fn run<Fut, T>(sim: &mut SimWorld, f: Fut) -> T
where
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handle = tokio::spawn(f);
    while !handle.is_finished() {
        while sim.pending_event_count() > 0 {
            sim.step();
        }
        tokio::task::yield_now().await;
    }
    handle.await.expect("task panicked")
}

fn id(position: u64) -> Id {
    let mut id = [0; ID_SIZE];
    id[..8].copy_from_slice(&position.to_le_bytes());
    id
}

async fn create(sim: &mut SimWorld, reads: &Arc<AtomicU64>) -> J {
    let provider = Counting {
        inner: sim.storage_provider("10.0.0.1".parse().expect("valid IP")),
        reads: Arc::clone(reads),
    };
    let config = JournalConfig {
        durability: Durability::Ordered,
        geometry: Geometry::small(),
        ..JournalConfig::default()
    };
    run(sim, async move {
        Journal::create(provider, DIR, JOURNAL, config, b"").await
    })
    .await
    .expect("create")
}

async fn put(sim: &mut SimWorld, mut journal: J, puts: &[(u64, usize)]) -> J {
    let mut batch = Batch::new();
    for &(position, len) in puts {
        batch.put(position, id(position), vec![7; len]);
    }
    run(
        sim,
        async move { journal.commit(batch).await.map(|()| journal) },
    )
    .await
    .expect("commit")
}

async fn verify(sim: &mut SimWorld, journal: J) -> (J, Vec<u64>) {
    let (journal, found) = run(sim, async move {
        let found = journal.verify_records(..).await;
        (journal, found)
    })
    .await;
    (journal, found.expect("verify"))
}

/// Flip one bit of `path` at `offset`, behind the journal's back.
async fn flip(sim: &mut SimWorld, path: String, offset: u64) {
    let provider = sim.storage_provider("10.0.0.1".parse().expect("valid IP"));
    run(sim, async move {
        let file = StorageProvider::open(&provider, &path, OpenOptions::read_write()).await?;
        let mut byte = [0u8; 1];
        StorageFile::read_at(&file, offset, &mut byte).await?;
        byte[0] ^= 0x10;
        StorageFile::write_at(&file, offset, &byte).await?;
        StorageFile::sync_all(&file).await
    })
    .await
    .expect("flip a bit");
}

/// A full persist block (64 records of one batch) is read with one read,
/// not one read per record (#491).
#[test]
fn a_scrub_reads_a_full_persist_block_once() {
    runtime().block_on(async {
        let mut sim = sim();
        let reads = Arc::new(AtomicU64::new(0));
        let journal = create(&mut sim, &reads).await;
        let puts: Vec<(u64, usize)> = (0..64).rev().map(|p| (p, 0)).collect();
        let journal = put(&mut sim, journal, &puts).await;
        let first = journal.layout(0).expect("live").record;
        let last = journal.layout(63).expect("live").record;
        assert_eq!(
            first.bytes.start / 4096,
            last.bytes.start / 4096,
            "one persist block"
        );
        let before = reads.load(Ordering::Relaxed);
        let (_, found) = verify(&mut sim, journal).await;
        assert_eq!(found, Vec::<u64>::new());
        assert_eq!(reads.load(Ordering::Relaxed) - before, 1, "one read");
    });
}

/// Positions out of disk order, across two segments: one rotted record
/// names its position only, and the result is in position order.
#[test]
fn a_scrub_out_of_disk_order_across_segments_names_one_rotted_record() {
    runtime().block_on(async {
        let mut sim = sim();
        let reads = Arc::new(AtomicU64::new(0));
        let mut journal = create(&mut sim, &reads).await;
        // Descending positions, two per batch, one entry block each: the
        // small geometry's 16 entry blocks roll onto a second segment.
        for round in 0..12u64 {
            let high = 1_000 - round * 10;
            journal = put(&mut sim, journal, &[(high, 1_000), (round, 1_000)]).await;
        }
        let paths: std::collections::BTreeSet<String> = journal
            .positions()
            .map(|(p, _)| journal.layout(p).expect("live").record.path)
            .collect();
        assert!(paths.len() >= 2, "two segments: {paths:?}");
        let (journal, clean) = verify(&mut sim, journal).await;
        assert_eq!(clean, Vec::<u64>::new());
        for victim in [3, 950] {
            let record = journal.layout(victim).expect("live").record;
            flip(&mut sim, record.path.clone(), record.bytes.start + 20).await;
        }
        let (_, found) = verify(&mut sim, journal).await;
        assert_eq!(found, vec![3, 950]);
    });
}
