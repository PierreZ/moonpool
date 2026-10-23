//! The compio backend: every local provider trait on a real compio runtime.

#![cfg(feature = "compio-providers")]

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::io::SeekFrom;
use std::rc::Rc;
use std::time::Duration;

use futures::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use moonpool_core::{
    AlignedBuf, CompioNetworkProvider, CompioProviders, CompioStorageProvider,
    CompioSystemProvider, CompioTaskProvider, CompioTimeProvider, Detach, DirectIo,
    LocalNetworkProvider, LocalProviders, LocalStorageFile, LocalStorageProvider,
    LocalTaskProvider, LocalTcpListener, LocalTimeProvider, OpenOptions, Resolver, SystemProvider,
    ThreadProviders, TimeError,
};
use tempfile::TempDir;

/// Run `future` to completion on a fresh compio runtime.
fn run<F: Future>(future: F) -> F::Output {
    compio::runtime::Runtime::new()
        .expect("compio runtime")
        .block_on(future)
}

fn scratch(dir: &TempDir, name: &str) -> String {
    dir.path()
        .join(name)
        .to_str()
        .expect("temp path is valid UTF-8")
        .to_owned()
}

#[test]
fn positioned_io_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = scratch(&dir, "positioned.bin");
    run(async {
        let storage = CompioStorageProvider::new();
        let file = storage
            .open(&path, OpenOptions::create_write().read(true))
            .await
            .expect("open");
        assert!(!file.is_direct_io());
        assert!(file.constraints().is_unconstrained());

        assert_eq!(file.write_at(4, b"world").await.expect("write"), 5);
        assert_eq!(file.write_at(0, b"hey ").await.expect("write"), 4);
        assert_eq!(file.write_at_durable(9, b"!").await.expect("durable"), 1);
        file.sync_all().await.expect("sync_all");
        assert_eq!(file.size().await.expect("size"), 10);

        let mut buf = [0_u8; 16];
        let read = file.read_at(0, &mut buf).await.expect("read");
        assert_eq!(&buf[..read], b"hey world!");
        assert_eq!(file.read_at(10, &mut buf).await.expect("eof"), 0);

        let mut owned = AlignedBuf::zeroed(5, 1);
        owned.as_mut_slice().copy_from_slice(b"WORLD");
        let (written, owned) = file.write_at_aligned(4, owned).await.expect("owned write");
        assert_eq!(written, 5);
        let (written, _) = file
            .write_at_durable_aligned(0, AlignedBuf::zeroed(1, 1))
            .await
            .expect("owned durable write");
        assert_eq!(written, 1);
        let (read, owned) = file.read_at_aligned(4, owned).await.expect("owned read");
        assert_eq!(&owned.as_slice()[..read], b"WORLD");

        file.set_len(3).await.expect("set_len");
        assert_eq!(file.size().await.expect("size"), 3);
        let read = file.read_at(0, &mut buf).await.expect("read");
        assert_eq!(&buf[..read], b"\0ey");
    });
}

#[test]
fn stream_io_shares_one_cursor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = scratch(&dir, "stream.bin");
    run(async {
        let storage = CompioStorageProvider::new();
        let mut file = storage
            .open(&path, OpenOptions::create_write().read(true))
            .await
            .expect("open");
        file.write_all(b"hello stream").await.expect("write_all");
        file.flush().await.expect("flush");
        assert_eq!(file.seek(SeekFrom::Current(0)).await.expect("seek"), 12);

        assert_eq!(file.seek(SeekFrom::Start(6)).await.expect("seek"), 6);
        let mut word = [0_u8; 6];
        file.read_exact(&mut word).await.expect("read_exact");
        assert_eq!(&word, b"stream");

        assert_eq!(file.seek(SeekFrom::End(-12)).await.expect("seek"), 0);
        let mut all = Vec::new();
        file.read_to_end(&mut all).await.expect("read_to_end");
        assert_eq!(all, b"hello stream");

        // A positioned write is visible to the stream, and moves no cursor.
        file.write_at(0, b"J").await.expect("write_at");
        file.seek(SeekFrom::Start(0)).await.expect("seek");
        let mut first = [0_u8; 5];
        file.read_exact(&mut first).await.expect("read_exact");
        assert_eq!(&first, b"Jello");
        assert!(file.seek(SeekFrom::Current(-6)).await.is_err());
    });
}

#[test]
fn append_streams_to_the_end_and_positions_stay_exact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = scratch(&dir, "append.log");
    run(async {
        let storage = CompioStorageProvider::new();
        let seed = storage
            .open(&path, OpenOptions::create_write())
            .await
            .expect("create");
        seed.write_at(0, b"0123").await.expect("seed");
        drop(seed);

        let mut log = storage
            .open(&path, OpenOptions::new().read(true).append(true))
            .await
            .expect("open append");
        log.write_all(b"ab").await.expect("append");
        // A positioned write on an append file goes where it is told.
        log.write_at(1, b"X").await.expect("positioned");
        log.write_all(b"cd").await.expect("append");
        let mut all = vec![0_u8; 16];
        let read = log.read_at(0, &mut all).await.expect("read");
        assert_eq!(&all[..read], b"0X23abcd");
    });
}

#[test]
fn opens_follow_the_shared_rules() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = scratch(&dir, "rules.bin");
    run(async {
        let storage = CompioStorageProvider::new();
        let invalid = storage
            .open(&path, OpenOptions::new().create(true))
            .await
            .expect_err("create without write is refused");
        assert_eq!(invalid.kind(), std::io::ErrorKind::InvalidInput);

        let refused = storage
            .open(
                &path,
                OpenOptions::create_write().direct_io(DirectIo::Required),
            )
            .await
            .expect_err("Required never creates a file");
        assert_eq!(refused.kind(), std::io::ErrorKind::Unsupported);
        assert!(
            !storage.exists(&path).await.expect("exists"),
            "a refused Required open leaves nothing behind"
        );

        let optional = storage
            .open(
                &path,
                OpenOptions::create_write().direct_io(DirectIo::Optional),
            )
            .await
            .expect("Optional creates the file, direct or not");
        assert_eq!(
            optional.is_direct_io(),
            !optional.constraints().is_unconstrained()
        );
        drop(optional);
        for policy in [DirectIo::Disabled, DirectIo::Optional, DirectIo::Required] {
            let kind = storage
                .open(&path, OpenOptions::create_new_write().direct_io(policy))
                .await
                .expect_err("create_new refuses an existing file under every policy")
                .kind();
            assert!(
                matches!(
                    kind,
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::Unsupported
                ),
                "{policy:?}: {kind:?}"
            );
        }

        let missing = storage
            .open(&scratch(&dir, "missing"), OpenOptions::read_only())
            .await
            .expect_err("a missing file does not open");
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);

        let exclusive = storage
            .open(&path, OpenOptions::create_new_write())
            .await
            .expect_err("create_new refuses an existing file");
        assert_eq!(exclusive.kind(), std::io::ErrorKind::AlreadyExists);
    });
}

/// Open `path` with `DirectIo::Required` after creating it the ordinary
/// way; `None` where this filesystem or platform has no direct I/O.
async fn open_direct(
    storage: &CompioStorageProvider,
    path: &str,
) -> Option<moonpool_core::CompioStorageFile> {
    let created = storage
        .open(path, OpenOptions::create_write())
        .await
        .expect("create");
    created.sync_all().await.expect("sync");
    drop(created);
    match storage
        .open(
            path,
            OpenOptions::read_write().direct_io(DirectIo::Required),
        )
        .await
    {
        Ok(file) => Some(file),
        Err(error) => {
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported, "{error}");
            eprintln!("skipped: direct I/O is unavailable here: {error}");
            None
        }
    }
}

/// A direct file advertises constraints its device accepts: transfers at
/// exactly that alignment go through, borrowed and owned; a misaligned one
/// and the stream API are refused.
#[test]
fn direct_io_transfers_at_the_reported_alignment() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = scratch(&dir, "direct.db");
    run(async {
        let storage = CompioStorageProvider::new();
        let Some(mut file) = open_direct(&storage, &path).await else {
            return;
        };
        assert!(file.is_direct_io());
        let constraints = file.constraints();
        assert!(!constraints.is_unconstrained());
        let length = constraints.length_alignment();
        let offset = constraints.offset_alignment();

        let mut block = AlignedBuf::for_constraints(length, constraints);
        block.as_mut_slice().fill(0xD1);
        let (written, block) = file
            .write_at_durable_aligned(offset, block)
            .await
            .expect("an aligned owned write is accepted");
        assert_eq!(written, length);
        let (read, back) = file
            .read_at_aligned(offset, AlignedBuf::for_constraints(length, constraints))
            .await
            .expect("an aligned owned read is accepted");
        assert_eq!(read, length);
        assert_eq!(back.as_slice(), block.as_slice());

        // The borrowed forms take an aligned slice and bounce it through an
        // aligned buffer of their own.
        let mut borrowed = AlignedBuf::for_constraints(length, constraints);
        assert_eq!(
            file.read_at(offset, borrowed.as_mut_slice())
                .await
                .expect("read_at"),
            length
        );
        assert_eq!(borrowed.as_slice(), block.as_slice());
        assert_eq!(
            file.write_at(0, borrowed.as_slice())
                .await
                .expect("write_at"),
            length
        );

        if offset > 1 {
            let misaligned = file
                .write_at(1, borrowed.as_slice())
                .await
                .expect_err("a misaligned offset is refused");
            assert_eq!(misaligned.kind(), std::io::ErrorKind::InvalidInput);
        }
        let stream = file
            .write_all(b"x")
            .await
            .expect_err("a direct file refuses stream writes");
        assert_eq!(stream.kind(), std::io::ErrorKind::Unsupported);
        let mut byte = [0_u8; 1];
        let stream = file
            .read(&mut byte)
            .await
            .expect_err("a direct file refuses stream reads");
        assert_eq!(stream.kind(), std::io::ErrorKind::Unsupported);
    });
}

/// `Required` with truncate truncates an existing file once direct I/O is
/// secured, and an `Optional` truncating open truncates exactly once.
#[test]
fn truncation_is_applied_once_under_every_policy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = scratch(&dir, "truncate.db");
    run(async {
        let storage = CompioStorageProvider::new();
        for policy in [DirectIo::Optional, DirectIo::Required] {
            let seed = storage
                .open(&path, OpenOptions::create_write())
                .await
                .expect("create");
            seed.write_at(0, b"old contents").await.expect("seed");
            drop(seed);
            match storage
                .open(
                    &path,
                    OpenOptions::read_write().truncate(true).direct_io(policy),
                )
                .await
            {
                Ok(file) => assert_eq!(file.size().await.expect("size"), 0, "{policy:?}"),
                Err(error) => {
                    assert_eq!(policy, DirectIo::Required, "{error}");
                    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
                    assert_eq!(
                        std::fs::metadata(&path).expect("metadata").len(),
                        12,
                        "a refused Required open leaves the file as it was"
                    );
                }
            }
        }
    });
}

/// Two append handles interleaving stream writes lose nothing: every write
/// lands whole at the end, which needs the kernel's `O_APPEND`. Positioned
/// writes on the same handles still go where they are told.
#[cfg(target_os = "linux")]
#[test]
fn appends_from_two_handles_never_overwrite_each_other() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = scratch(&dir, "shared.log");
    run(async {
        let storage = CompioStorageProvider::new();
        let append = || OpenOptions::new().read(true).append(true).create(true);
        let mut first = storage.open(&path, append()).await.expect("open");
        let mut second = storage.open(&path, append()).await.expect("open");
        for round in 0..8_u8 {
            first.write_all(&[b'a' + round; 3]).await.expect("append");
            second.write_all(&[b'A' + round; 2]).await.expect("append");
        }
        first.flush().await.expect("flush");
        second.flush().await.expect("flush");
        assert_eq!(first.size().await.expect("size"), 8 * 5);

        // The stream cursor of an append handle is the end of the file.
        assert_eq!(first.seek(SeekFrom::Current(0)).await.expect("seek"), 40);
        assert_eq!(second.seek(SeekFrom::Current(-2)).await.expect("seek"), 38);
        let mut tail = Vec::new();
        second.read_to_end(&mut tail).await.expect("read_to_end");
        assert_eq!(tail, b"HH");

        first.write_at(0, b"!").await.expect("positioned");
        let mut all = vec![0_u8; 64];
        let read = first.read_at(0, &mut all).await.expect("read");
        all.truncate(read);
        let mut expected = Vec::new();
        for round in 0..8_u8 {
            expected.extend_from_slice(&[b'a' + round; 3]);
            expected.extend_from_slice(&[b'A' + round; 2]);
        }
        expected[0] = b'!';
        assert_eq!(all, expected);
    });
}

/// The stream and positioned sides of one append handle stay on the file it
/// opened, even after a replacement is renamed over its path.
#[test]
fn an_append_handle_stays_on_one_file_across_a_rename() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = scratch(&dir, "append.db");
    let replacement = scratch(&dir, "replacement.db");
    run(async {
        let storage = CompioStorageProvider::new();
        let seed = storage
            .open(&path, OpenOptions::create_write())
            .await
            .expect("create");
        seed.write_at(0, b"AAAA").await.expect("seed");
        drop(seed);
        let mut file = storage
            .open(&path, OpenOptions::new().read(true).append(true))
            .await
            .expect("open");
        let other = storage
            .open(&replacement, OpenOptions::create_write())
            .await
            .expect("create");
        other.write_at(0, b"ZZZZ").await.expect("write");
        drop(other);
        storage.rename(&replacement, &path).await.expect("rename");

        file.write_all(b"SS").await.expect("append");
        file.write_at(0, b"P").await.expect("positioned");
        let mut buf = [0_u8; 8];
        let read = file.read_at(0, &mut buf).await.expect("read");
        assert_eq!(&buf[..read], b"PAAASS");
        assert_eq!(std::fs::read(&path).expect("read"), b"ZZZZ");
    });
}

#[test]
fn the_namespace_operations_work() {
    let dir = tempfile::tempdir().expect("tempdir");
    let nested = scratch(&dir, "a/b");
    run(async {
        let storage = CompioStorageProvider::new();
        storage
            .create_dir_all(&nested)
            .await
            .expect("create_dir_all");
        let first = format!("{nested}/first");
        let second = format!("{nested}/second");
        storage
            .open(&first, OpenOptions::create_write())
            .await
            .expect("create");
        assert!(storage.exists(&first).await.expect("exists"));
        storage.rename(&first, &second).await.expect("rename");
        assert!(!storage.exists(&first).await.expect("exists"));
        storage
            .open(&format!("{nested}/alpha"), OpenOptions::create_write())
            .await
            .expect("create");
        assert_eq!(
            storage.list_dir(&nested).await.expect("list_dir"),
            vec!["alpha".to_string(), "second".to_string()]
        );
        storage.sync_dir(&nested).await.expect("sync_dir");
        let not_dir = storage.sync_dir(&second).await.expect_err("a file");
        assert_eq!(not_dir.kind(), std::io::ErrorKind::NotADirectory);
        storage.delete(&second).await.expect("delete");
        assert!(!storage.exists(&second).await.expect("exists"));
    });
}

#[test]
fn tcp_echoes_bytes() {
    run(async {
        let network = CompioNetworkProvider::new();
        let listener = network.bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = CompioTaskProvider.spawn_local_task("echo", async move {
            let (mut stream, _peer) = listener.accept().await.expect("accept");
            let mut buf = [0_u8; 5];
            stream.read_exact(&mut buf).await.expect("server read");
            stream.write_all(&buf).await.expect("server write");
            stream.flush().await.expect("server flush");
        });
        let mut client = network.connect(&address).await.expect("connect");
        client.write_all(b"hello").await.expect("client write");
        client.flush().await.expect("client flush");
        let mut echoed = [0_u8; 5];
        client.read_exact(&mut echoed).await.expect("client read");
        assert_eq!(&echoed, b"hello");
        server.await.expect("server task");
    });
}

#[test]
fn time_sleeps_and_times_out() {
    run(async {
        let time = CompioTimeProvider::new();
        time.sleep(Duration::from_millis(2)).await.expect("sleep");
        assert!(time.now() >= Duration::from_millis(2));

        let shared = Rc::new(Cell::new(1));
        let inner = Rc::clone(&shared);
        let done = time
            .timeout(Duration::from_secs(5), async move {
                inner.set(2);
                inner.get()
            })
            .await;
        assert_eq!(done, Ok(2));
        let elapsed = time
            .timeout(Duration::from_millis(1), futures::future::pending::<()>())
            .await;
        assert_eq!(elapsed, Err(TimeError::Elapsed));
        assert_eq!(shared.get(), 2);
    });
}

#[test]
fn tasks_hold_rc_and_detach_on_drop() {
    run(async {
        let tasks = CompioTaskProvider;
        let log = Rc::new(RefCell::new(Vec::new()));

        let inner = Rc::clone(&log);
        tasks
            .spawn_local_task("joined", async move {
                inner.borrow_mut().push("joined");
                LocalTaskProvider::yield_now(&CompioTaskProvider).await;
            })
            .await
            .expect("task completed");

        // Dropping the handle detaches the task rather than cancelling it.
        let inner = Rc::clone(&log);
        drop(tasks.spawn_local_task("dropped", async move {
            inner.borrow_mut().push("dropped");
        }));
        let inner = Rc::clone(&log);
        tasks
            .spawn_local_task("detached", async move {
                inner.borrow_mut().push("detached");
            })
            .detach();
        for _ in 0..4 {
            tasks.yield_now().await;
        }
        assert_eq!(*log.borrow(), vec!["joined", "dropped", "detached"]);

        let panicked = tasks
            .spawn_local_task("panics", async { panic!("expected test panic") })
            .await;
        assert!(matches!(panicked, Err(moonpool_core::JoinError::Panicked)));
    });
}

/// Code written once against the local bundle.
async fn bundle_round_trip<P: LocalProviders>(
    providers: &P,
    path: &str,
) -> std::io::Result<Vec<u8>> {
    providers
        .time()
        .sleep(Duration::from_millis(1))
        .await
        .map_err(std::io::Error::other)?;
    let file = providers
        .storage()
        .open(path, OpenOptions::create_write().read(true))
        .await?;
    file.write_at(0, b"compio").await?;
    file.sync_data().await?;
    let mut buf = vec![0; 6];
    let read = file.read_at(0, &mut buf).await?;
    buf.truncate(read);
    Ok(buf)
}

#[test]
fn the_bundle_runs_generic_local_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = scratch(&dir, "bundle.bin");
    let read =
        run(async { bundle_round_trip(&CompioProviders::new(), &path).await }).expect("round trip");
    assert_eq!(read, b"compio");
}

#[test]
fn system_samples_this_process() {
    let provider = CompioSystemProvider::new();
    let first = provider.sample(".").expect("sample");
    assert!(first.cores >= 1);
    let mut spin = 0_u64;
    for i in 0..2_000_000_u64 {
        spin = spin.wrapping_add(i * i);
    }
    assert_ne!(spin, 1);
    let second = provider.sample(".").expect("sample");
    // The run loop is the calling thread: its CPU time only grows.
    let (Some(before), Some(after)) = (first.run_loop_busy, second.run_loop_busy) else {
        panic!("the thread's CPU time is readable on this target");
    };
    assert!(after >= before);
}

/// `CompioProviders` is a `ThreadProviders` bundle: generic code spawns
/// `!Send` tasks through its local task slot.
#[test]
fn the_bundle_spawns_local_tasks() {
    async fn bump<P: ThreadProviders>(providers: &P, counter: Rc<Cell<u32>>) {
        providers
            .local_task()
            .spawn_local_task("bump", async move { counter.set(counter.get() + 1) })
            .await
            .expect("the local task completes");
    }
    run(async {
        let counter = Rc::new(Cell::new(0));
        bump(&CompioProviders::new(), Rc::clone(&counter)).await;
        assert_eq!(counter.get(), 1);
    });
}

/// `CompioResolver` answers literals and `localhost` from the compio
/// thread, without blocking it.
#[test]
fn the_resolver_answers_literals_and_localhost() {
    run(async {
        let resolver = moonpool_core::CompioResolver::new();
        let literal = resolver.resolve("127.0.0.1:4500").await.expect("a literal");
        assert_eq!(
            literal,
            vec!["127.0.0.1:4500".parse().expect("literal address")]
        );
        let localhost = resolver.resolve("localhost:4500").await;
        assert!(localhost.is_ok_and(|found| found.iter().all(|a| a.port() == 4500)));
        assert!(resolver.resolve("no-port").await.is_err());
    });
}
