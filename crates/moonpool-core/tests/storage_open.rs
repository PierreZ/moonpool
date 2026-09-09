//! Open semantics of the production `StorageProvider`.
//!
//! The direct-I/O policy must not change what an open *means*. These tests
//! pin the boundary: `Optional` may fall back to buffered I/O, and may not
//! turn any other failure into a success, weaken `create_new`, or create a
//! file the caller did not ask it to create.

use std::os::unix::fs::MetadataExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use moonpool_core::{
    AlignedBuf, DirectIo, OpenOptions, StorageFile, StorageProvider, TokioStorageProvider,
};
use tempfile::TempDir;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build runtime")
}

/// `create_new` means "fail if it exists" under every direct-I/O policy. The
/// bug this pins: attempting `O_DIRECT` first, having the kernel create the
/// file and *then* reject the flag, and retrying without the exclusivity the
/// caller asked for — which silently opens the existing file instead.
#[test]
fn optional_direct_io_does_not_weaken_create_new() {
    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("exclusive.db");
        let path = path.to_str().expect("temp path is valid UTF-8");
        let provider = TokioStorageProvider::new();

        // Someone else already owns this name, with contents.
        let first = provider
            .open(path, OpenOptions::create_new_write())
            .await
            .expect("first create_new must succeed");
        first.write_at(0, b"original").await.expect("write failed");
        first.sync_all().await.expect("sync failed");
        drop(first);

        for policy in [DirectIo::Disabled, DirectIo::Optional, DirectIo::Required] {
            let error = provider
                .open(path, OpenOptions::create_new_write().direct_io(policy))
                .await
                .err()
                .map(|error| error.kind());
            assert!(
                matches!(
                    error,
                    Some(std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::Unsupported)
                ),
                "create_new on an existing file must fail under {policy:?}, got {error:?}"
            );
        }

        // And the contents are still there: no attempt truncated or replaced
        // the file it refused to create.
        let survivor = provider
            .open(path, OpenOptions::read_only())
            .await
            .expect("open failed");
        let mut buf = [0u8; 8];
        assert_eq!(survivor.read_at(0, &mut buf).await.expect("read"), 8);
        assert_eq!(&buf, b"original");
    });
}

/// An ordinary open failure stays that failure. `Optional` must not retry its
/// way into a success, nor create a file for a caller that did not ask for
/// one.
#[test]
fn optional_direct_io_preserves_ordinary_open_failures() {
    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let missing = dir.path().join("nonexistent.db");
        let missing = missing.to_str().expect("temp path is valid UTF-8");
        let provider = TokioStorageProvider::new();

        let error = provider
            .open(
                missing,
                OpenOptions::read_only().direct_io(DirectIo::Optional),
            )
            .await
            .expect_err("opening a missing file without create must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(
            !provider.exists(missing).await.expect("exists failed"),
            "a failed open must not leave a file behind"
        );

        // A directory is not a file to write to, under any policy.
        let as_file = dir.path().to_str().expect("temp path is valid UTF-8");
        let error = provider
            .open(
                as_file,
                OpenOptions::new().write(true).direct_io(DirectIo::Optional),
            )
            .await
            .expect_err("opening a directory for writing must fail");
        assert_ne!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "the directory exists; the failure is about writing to it"
        );
    });
}

/// A truncating open truncates exactly once. If the direct attempt were made
/// first and then retried, the file would be truncated twice — harmless here,
/// but the same double-application is what makes `create_new` unsafe.
#[test]
fn optional_direct_io_applies_truncation_once() {
    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("truncated.db");
        let path = path.to_str().expect("temp path is valid UTF-8");
        let provider = TokioStorageProvider::new();

        let seeded = provider
            .open(path, OpenOptions::create_write())
            .await
            .expect("open failed");
        seeded.write_at(0, &[0xAB; 4096]).await.expect("write");
        seeded.sync_all().await.expect("sync");
        drop(seeded);

        let reopened = provider
            .open(
                path,
                OpenOptions::create_write()
                    .read(true)
                    .direct_io(DirectIo::Optional),
            )
            .await
            .expect("open failed");
        assert_eq!(reopened.size().await.expect("size"), 0);

        // The handle is usable whichever way the upgrade went — through a
        // buffer built for whatever constraints it ended up with.
        let constraints = reopened.constraints();
        let block = AlignedBuf::for_constraints(constraints.length_alignment(), constraints);
        reopened
            .write_at(0, block.as_slice())
            .await
            .expect("write failed");
    });
}

/// A direct-I/O file must advertise constraints its own device will honour.
/// The alignment is discovered, not assumed: this proves the reported values
/// are actually accepted, which a hard-coded guess cannot.
#[test]
fn direct_io_constraints_describe_what_the_device_accepts() {
    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("aligned.db");
        let path = path.to_str().expect("temp path is valid UTF-8");
        let provider = TokioStorageProvider::new();

        let file = provider
            .open(
                path,
                OpenOptions::create_write()
                    .read(true)
                    .direct_io(DirectIo::Optional),
            )
            .await
            .expect("open failed");

        let constraints = file.constraints();
        if !file.is_direct_io() {
            assert!(
                constraints.is_unconstrained(),
                "a buffered file constrains nothing"
            );
            return;
        }

        for alignment in [
            constraints.offset_alignment(),
            constraints.length_alignment() as u64,
            constraints.memory_alignment() as u64,
        ] {
            assert!(
                alignment.is_power_of_two(),
                "{alignment} is not a power of two"
            );
        }

        // A transfer built to exactly the reported alignment must be accepted
        // by the device. If the numbers were too weak, this is where the
        // kernel would return EINVAL.
        let length = constraints.length_alignment();
        let mut block = AlignedBuf::for_constraints(length, constraints);
        block.as_mut_slice().fill(0xE1);
        assert_eq!(
            file.write_at(constraints.offset_alignment(), block.as_slice())
                .await
                .expect("a transfer at the reported alignment must be accepted"),
            length
        );

        let mut read = AlignedBuf::for_constraints(length, constraints);
        assert_eq!(
            file.read_at(constraints.offset_alignment(), read.as_mut_slice())
                .await
                .expect("read failed"),
            length
        );
        assert_eq!(read.as_slice(), block.as_slice());
    });
}

/// The stream API and alignment are mutually exclusive, and a direct-I/O file
/// says so itself rather than letting the kernel answer with `EINVAL` on some
/// requests and succeed on others.
#[test]
fn a_direct_io_file_refuses_stream_io() {
    use futures::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("stream.db");
        let path = path.to_str().expect("temp path is valid UTF-8");
        let provider = TokioStorageProvider::new();

        let mut file = match provider
            .open(
                path,
                OpenOptions::create_write()
                    .read(true)
                    .direct_io(DirectIo::Required),
            )
            .await
        {
            Ok(file) => file,
            // A filesystem without direct I/O has nothing to say here.
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => return,
            Err(error) => panic!("open failed: {error}"),
        };

        let constraints = file.constraints();
        let mut aligned = AlignedBuf::for_constraints(constraints.length_alignment(), constraints);

        // Even a perfectly aligned buffer is refused: it is the shared cursor
        // that cannot be kept aligned, not this particular transfer.
        let error = file
            .write(aligned.as_slice())
            .await
            .expect_err("stream writes must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);

        let error = file
            .read(aligned.as_mut_slice())
            .await
            .expect_err("stream reads must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);

        // Seeking moves a cursor; it transfers nothing, so it stays available.
        assert_eq!(
            file.seek(std::io::SeekFrom::Start(0))
                .await
                .expect("seeking must stay available"),
            0
        );

        // And positioned I/O works throughout.
        assert_eq!(
            file.write_at(0, aligned.as_slice())
                .await
                .expect("positioned writes are what this file is for"),
            aligned.len()
        );
    });
}

/// A buffered file keeps the ordinary stream semantics; nothing here narrows
/// what a file without constraints can do.
#[test]
fn a_buffered_file_keeps_stream_io() {
    use futures::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("buffered.db");
        let path = path.to_str().expect("temp path is valid UTF-8");
        let provider = TokioStorageProvider::new();

        let mut file = provider
            .open(path, OpenOptions::create_write().read(true))
            .await
            .expect("open failed");
        assert!(file.constraints().is_unconstrained());

        file.write_all(b"streamed").await.expect("write failed");
        file.seek(std::io::SeekFrom::Start(0)).await.expect("seek");
        let mut buf = [0u8; 8];
        file.read_exact(&mut buf).await.expect("read failed");
        assert_eq!(&buf, b"streamed");
    });
}

/// `write_at` is positioned even on a file opened for appending.
///
/// The two halves of such a file mean different things: a stream write goes to
/// the end because that is what append is for, and a positioned write goes
/// exactly where it is told because that is what `write_at` promises. Sharing
/// one descriptor between them cannot deliver both — `pwrite` to an
/// `O_APPEND` descriptor appends and ignores the offset — so this is the test
/// that the two are actually separate.
#[test]
fn positioned_writes_ignore_append_on_a_real_file() {
    use futures::io::AsyncWriteExt;

    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("append.db");
        let path = path.to_str().expect("temp path is valid UTF-8");
        let provider = TokioStorageProvider::new();

        let seed = provider
            .open(path, OpenOptions::create_write())
            .await
            .expect("open failed");
        seed.write_at(0, b"AAAAAAAA").await.expect("seed write");
        seed.sync_all().await.expect("sync failed");
        drop(seed);

        let mut file = provider
            .open(path, OpenOptions::new().read(true).write(true).append(true))
            .await
            .expect("open failed");

        // Positioned: exactly at the offset, overwriting in place.
        assert_eq!(file.write_at(0, b"BB").await.expect("write_at failed"), 2);
        let mut buf = [0u8; 8];
        assert_eq!(file.read_at(0, &mut buf).await.expect("read_at failed"), 8);
        assert_eq!(
            &buf, b"BBAAAAAA",
            "a positioned write on an append file must not append"
        );
        assert_eq!(
            file.size().await.expect("size failed"),
            8,
            "and must not extend the file"
        );

        // Stream: at the end, because that is what append means.
        file.write_all(b"CC").await.expect("stream write failed");
        file.flush().await.expect("flush failed");

        let mut all = [0u8; 10];
        assert_eq!(file.read_at(0, &mut all).await.expect("read_at failed"), 10);
        assert_eq!(
            &all, b"BBAAAAAACC",
            "an ordinary stream write on an append file must append"
        );
    });
}

/// A directory whose filesystem refuses `O_DIRECT`, if this machine has one.
///
/// `MOONPOOL_NO_DIRECT_IO_DIR` names it explicitly; otherwise any `ramfs`
/// mount will do, since ramfs has no direct-I/O path at all. Most machines
/// have neither, so the assertions that need one are reported as skipped
/// rather than silently passing.
fn directory_refusing_direct_io() -> Option<std::path::PathBuf> {
    if let Ok(configured) = std::env::var("MOONPOOL_NO_DIRECT_IO_DIR") {
        return Some(std::path::PathBuf::from(configured));
    }
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
    mounts.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let _device = fields.next()?;
        let mount = fields.next()?;
        let kind = fields.next()?;
        (kind == "ramfs").then(|| std::path::PathBuf::from(mount))
    })
}

/// A failed `Required` open must leave the filesystem exactly as it found it.
///
/// Linux applies `O_CREAT` and `O_TRUNC` *before* it rejects `O_DIRECT`, so an
/// open that carried both in one call created a file, or emptied one, and then
/// reported failure. `Required` cannot fall back, so the lifecycle has to wait
/// until direct I/O is actually in hand.
#[test]
fn a_refused_required_open_applies_no_lifecycle() {
    let Some(dir) = directory_refusing_direct_io() else {
        eprintln!(
            "skipped: no filesystem here refuses O_DIRECT; \
             set MOONPOOL_NO_DIRECT_IO_DIR to a ramfs mount to run it"
        );
        return;
    };

    runtime().block_on(async {
        let provider = TokioStorageProvider::new();
        let unique = std::process::id();

        // create_new: the file must not be left behind.
        let created = dir.join(format!("required-create-new-{unique}"));
        let created = created.to_str().expect("path is valid UTF-8");
        let _ = std::fs::remove_file(created);
        let error = provider
            .open(
                created,
                OpenOptions::create_new_write().direct_io(DirectIo::Required),
            )
            .await
            .expect_err("direct I/O is unavailable here, so the open must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert!(
            !provider.exists(created).await.expect("exists failed"),
            "a refused create_new must not leave a file behind"
        );

        // plain create: likewise.
        let error = provider
            .open(
                created,
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .direct_io(DirectIo::Required),
            )
            .await
            .expect_err("the open must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert!(
            !provider.exists(created).await.expect("exists failed"),
            "a refused create must not leave a file behind"
        );

        // truncate: the previous contents must survive.
        let existing = dir.join(format!("required-truncate-{unique}"));
        let existing_str = existing.to_str().expect("path is valid UTF-8");
        std::fs::write(&existing, b"PRECIOUS").expect("seed write failed");

        let error = provider
            .open(
                existing_str,
                OpenOptions::create_write().direct_io(DirectIo::Required),
            )
            .await
            .expect_err("the open must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert_eq!(
            std::fs::read(&existing).expect("read failed"),
            b"PRECIOUS",
            "a refused truncating open must not destroy the contents"
        );

        let _ = std::fs::remove_file(&existing);
    });
}

/// The lifecycle `Required` defers is still applied — in the right order —
/// once direct I/O is secured. Runs wherever direct I/O works.
#[test]
fn a_successful_required_open_still_applies_the_lifecycle() {
    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let provider = TokioStorageProvider::new();

        let missing = dir.path().join("absent.db");
        let missing = missing.to_str().expect("temp path is valid UTF-8");

        // No create: NotFound, and nothing brought into existence.
        let error = provider
            .open(
                missing,
                OpenOptions::read_only().direct_io(DirectIo::Required),
            )
            .await
            .expect_err("a missing file without create must fail");
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::Unsupported
        ));
        assert!(!provider.exists(missing).await.expect("exists failed"));

        let path = dir.path().join("lifecycle.db");
        let path = path.to_str().expect("temp path is valid UTF-8");
        let Ok(created) = provider
            .open(
                path,
                OpenOptions::create_new_write()
                    .read(true)
                    .direct_io(DirectIo::Required),
            )
            .await
        else {
            eprintln!("skipped: direct I/O is unavailable on this filesystem");
            return;
        };
        let constraints = created.constraints();
        let mut block = AlignedBuf::for_constraints(constraints.length_alignment(), constraints);
        block.as_mut_slice().fill(0xD7);
        created.write_at(0, block.as_slice()).await.expect("write");
        created.sync_all().await.expect("sync");
        assert_eq!(created.size().await.expect("size"), block.len() as u64);
        drop(created);

        // create_new against the now-existing file: refused, contents intact.
        let error = provider
            .open(
                path,
                OpenOptions::create_new_write().direct_io(DirectIo::Required),
            )
            .await
            .expect_err("create_new on an existing file must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);

        let reopened = provider
            .open(path, OpenOptions::read_only().direct_io(DirectIo::Required))
            .await
            .expect("open failed");
        assert_eq!(
            reopened.size().await.expect("size"),
            block.len() as u64,
            "a refused create_new must not have truncated the file"
        );
        drop(reopened);

        // truncate: deferred until after direct I/O is secured, but applied.
        let truncated = provider
            .open(
                path,
                OpenOptions::create_write()
                    .read(true)
                    .direct_io(DirectIo::Required),
            )
            .await
            .expect("open failed");
        assert_eq!(
            truncated.size().await.expect("size"),
            0,
            "a truncating open must still truncate"
        );
    });
}

/// A failed `Required` create must not create the target name at all — which
/// is the only way it can be guaranteed never to remove somebody else's.
///
/// A rollback cannot give that guarantee: an exclusive create proves the inode
/// was ours when it was made, not that the name still refers to it when the
/// cleanup runs, so a concurrent unlink-and-recreate turns the rollback into a
/// deletion of another process's file. This is the deterministic half of the
/// evidence — the containing directory must come back **unmodified**, meaning
/// no entry was ever added or removed. A create-then-unlink leaves two
/// modifications behind and fails here.
#[test]
fn a_failed_required_create_never_touches_the_directory() {
    let Some(dir) = directory_refusing_direct_io() else {
        eprintln!(
            "skipped: no filesystem here refuses O_DIRECT; \
             set MOONPOOL_NO_DIRECT_IO_DIR to a ramfs mount to run it"
        );
        return;
    };

    let scratch = dir.join(format!("untouched-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir(&scratch).expect("create scratch dir failed");
    // A bystander, so the directory is not trivially empty.
    std::fs::write(scratch.join("bystander"), b"UNRELATED").expect("seed write failed");

    let before = std::fs::metadata(&scratch)
        .expect("stat failed")
        .modified()
        .expect("mtime unavailable");
    // Directory timestamps have finite resolution; make sure any modification
    // the open makes lands strictly after the snapshot.
    std::thread::sleep(std::time::Duration::from_millis(20));

    runtime().block_on(async {
        let provider = TokioStorageProvider::new();
        let target = scratch.join("wanted.db");
        let target = target.to_str().expect("path is valid UTF-8");

        for options in [
            OpenOptions::create_new_write().direct_io(DirectIo::Required),
            OpenOptions::new()
                .write(true)
                .create(true)
                .direct_io(DirectIo::Required),
        ] {
            let error = provider
                .open(target, options)
                .await
                .expect_err("direct I/O is unavailable here");
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        }
    });

    let after = std::fs::metadata(&scratch)
        .expect("stat failed")
        .modified()
        .expect("mtime unavailable");
    let entries: Vec<_> = std::fs::read_dir(&scratch)
        .expect("read_dir failed")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    let _ = std::fs::remove_dir_all(&scratch);

    assert_eq!(
        entries.len(),
        1,
        "the directory must hold only the bystander, found {entries:?}"
    );
    assert_eq!(
        before, after,
        "a failed create modified the directory: it created a name, \
         and a name it created is a name it has to remove"
    );
}

/// The racing half of the same evidence: a competitor that takes the contested
/// name over and installs its own file must never find that file removed by
/// somebody else.
///
/// This is the reviewer's sequence directly — B unlinks the name, B creates
/// its own file, and a rollback in flight then deletes it. The assertion is
/// one-sided: it can only fire if the bug is present.
#[test]
fn a_failed_required_create_never_removes_another_processs_file() {
    let Some(dir) = directory_refusing_direct_io() else {
        eprintln!("skipped: no filesystem here refuses O_DIRECT");
        return;
    };

    let contested = dir.join(format!("contested-{}", std::process::id()));
    let _ = std::fs::remove_file(&contested);

    let stop = Arc::new(AtomicBool::new(false));
    let stolen = Arc::new(AtomicUsize::new(0));
    let installed = Arc::new(AtomicUsize::new(0));

    // One competitor, behaving like the review's process B: take the name
    // over, install its own file, and check that nothing else removes it.
    // Exactly one, so that anything happening to its file is the opener's
    // doing and not another competitor's.
    let competitor = {
        let contested = contested.clone();
        let stop = Arc::clone(&stop);
        let stolen = Arc::clone(&stolen);
        let installed = Arc::clone(&installed);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // B unlinks whatever is at the name, then installs its own.
                let _ = std::fs::remove_file(&contested);
                let Ok(_) = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&contested)
                else {
                    continue;
                };
                let Ok(mine) = std::fs::metadata(&contested) else {
                    stolen.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                installed.fetch_add(1, Ordering::Relaxed);

                std::thread::yield_now();

                match std::fs::metadata(&contested) {
                    // Gone, and this thread did not remove it.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        stolen.fetch_add(1, Ordering::Relaxed);
                    }
                    // Still there, but no longer the same file.
                    Ok(now) if now.ino() != mine.ino() => {
                        stolen.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        })
    };

    runtime().block_on(async {
        let provider = TokioStorageProvider::new();
        let contested = contested.to_str().expect("path is valid UTF-8");
        for _ in 0..4_000 {
            // Direct I/O is unavailable here, so every one of these fails.
            // What matters is what the failure leaves behind.
            let _ = provider
                .open(
                    contested,
                    OpenOptions::new()
                        .write(true)
                        .create(true)
                        .direct_io(DirectIo::Required),
                )
                .await;
        }
    });

    stop.store(true, Ordering::Relaxed);
    competitor.join().expect("competitor thread panicked");
    let _ = std::fs::remove_file(&contested);

    assert!(
        installed.load(Ordering::Relaxed) > 0,
        "the competitor never installed a file, so nothing was tested"
    );
    assert_eq!(
        stolen.load(Ordering::Relaxed),
        0,
        "a failed Required create removed or replaced a file it did not create"
    );
}

/// A `Required` create must not replace a file another process installed at
/// the target path, and `create_new` must report that it lost the name.
///
/// Publication is a link, which fails rather than replacing, so the winner's
/// file survives untouched either way — `create_new` refuses, and a plain
/// `create` opens what is there.
#[test]
fn required_create_never_replaces_a_concurrent_winner() {
    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("contested.db");
        let path_str = path.to_str().expect("temp path is valid UTF-8");
        let provider = TokioStorageProvider::new();

        // The winner is already there, with contents.
        std::fs::write(&path, b"WINNER").expect("seed write failed");

        let refused = provider
            .open(
                path_str,
                OpenOptions::create_new_write().direct_io(DirectIo::Required),
            )
            .await
            .err()
            .map(|error| error.kind());
        assert!(
            matches!(
                refused,
                Some(std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::Unsupported)
            ),
            "create_new must lose to the file that is already there, got {refused:?}"
        );
        assert_eq!(
            std::fs::read(&path).expect("read failed"),
            b"WINNER",
            "create_new must not have replaced or truncated the winner"
        );

        // A plain create opens what is there rather than replacing it.
        match provider
            .open(
                path_str,
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .direct_io(DirectIo::Required),
            )
            .await
        {
            Ok(_) | Err(_) => {}
        }
        assert_eq!(
            std::fs::read(&path).expect("read failed"),
            b"WINNER",
            "a plain create must open the existing file, never overwrite it"
        );
    });
}

/// A failed `Required` create leaves nothing behind — neither the target name
/// nor a staging file.
#[test]
fn a_failed_required_create_leaks_no_staging_file() {
    let Some(dir) = directory_refusing_direct_io() else {
        eprintln!("skipped: no filesystem here refuses O_DIRECT");
        return;
    };

    let scratch = dir.join(format!("staging-scratch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir(&scratch).expect("create scratch dir failed");

    runtime().block_on(async {
        let provider = TokioStorageProvider::new();
        let target = scratch.join("wanted.db");
        let target = target.to_str().expect("path is valid UTF-8");

        for _ in 0..16 {
            let error = provider
                .open(
                    target,
                    OpenOptions::create_new_write().direct_io(DirectIo::Required),
                )
                .await
                .expect_err("direct I/O is unavailable here");
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        }
    });

    let leftovers: Vec<_> = std::fs::read_dir(&scratch)
        .expect("read_dir failed")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        leftovers.is_empty(),
        "a failed create left files behind: {leftovers:?}"
    );
}
