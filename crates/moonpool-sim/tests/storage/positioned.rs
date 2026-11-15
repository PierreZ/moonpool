//! Positioned file I/O: `read_at` / `write_at`.
//!
//! These are the Phase-1 acceptance tests for the primitive a database
//! actually wants — read and write at an explicit offset, with no shared seek
//! cursor and no pretence of being `read_exact`/`write_all`.

use crate::{fast_sim, local_runtime, run_storage_test, step_until_done, test_ip};
use moonpool_core::FileCursor;
use moonpool_core::{AlignedBuf, DirectIo, OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration};
use std::io::SeekFrom;

/// Read back exactly what a positioned write put down.
#[test]
fn positioned_write_then_positioned_read() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            let file = provider
                .open("positioned.db", OpenOptions::create_write().read(true))
                .await?;
            file.set_len(4096).await?;

            let written = file.write_at(1024, b"page-one").await?;
            assert_eq!(written, b"page-one".len());

            let mut buf = vec![0u8; 8];
            let read = file.read_at(1024, &mut buf).await?;
            assert_eq!(read, 8);
            assert_eq!(&buf, b"page-one");
            Ok(())
        })
        .await;
        result.expect("positioned I/O failed");
    });
}

/// One file image: a positioned write is visible to a cursor read, and a
/// cursor write is visible to a positioned read.
#[test]
fn positioned_and_cursor_io_share_one_file_image() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            let mut file = FileCursor::new(
                provider
                    .open("shared.db", OpenOptions::create_write().read(true))
                    .await?,
            );

            // Stream write, positioned read.
            file.write_all(b"streamed").await?;
            let mut buf = vec![0u8; 8];
            assert_eq!(file.read_at(0, &mut buf).await?, 8);
            assert_eq!(&buf, b"streamed");

            // Positioned write, cursor read.
            file.write_at(0, b"POSITION").await?;
            file.seek(SeekFrom::Start(0)).await?;
            let mut streamed = vec![0u8; 8];
            file.read_exact(&mut streamed).await?;
            assert_eq!(&streamed, b"POSITION");
            Ok(())
        })
        .await;
        result.expect("shared image test failed");
    });
}

/// Two handles on one path observe one file, whichever API they use.
#[test]
fn two_handles_share_file_state() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            let writer = provider
                .open("shared-handles.db", OpenOptions::create_write())
                .await?;
            writer.set_len(512).await?;
            writer.write_at(64, b"from-writer").await?;

            let reader = provider
                .open("shared-handles.db", OpenOptions::read_only())
                .await?;
            let mut buf = vec![0u8; b"from-writer".len()];
            assert_eq!(reader.read_at(64, &mut buf).await?, buf.len());
            assert_eq!(&buf, b"from-writer");
            assert_eq!(reader.size().await?, 512);
            Ok(())
        })
        .await;
        result.expect("shared handle test failed");
    });
}

/// A read that starts inside the file but asks for more than is there returns
/// what is there; a read that starts at or past the end returns 0 (EOF), which
/// is distinct from a short read.
#[test]
fn positioned_reads_are_short_at_end_of_file() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            let file = provider
                .open("eof.db", OpenOptions::create_write().read(true))
                .await?;
            file.write_at(0, b"twelve bytes").await?;

            let mut buf = vec![0u8; 64];
            let read = file.read_at(4, &mut buf).await?;
            assert_eq!(read, b"twelve bytes".len() - 4);
            assert_eq!(&buf[..read], b"ve bytes");

            assert_eq!(file.read_at(12, &mut buf).await?, 0, "EOF reads nothing");
            assert_eq!(
                file.read_at(999, &mut buf).await?,
                0,
                "past EOF reads nothing"
            );
            Ok(())
        })
        .await;
        result.expect("EOF test failed");
    });
}

/// With the short-transfer family armed, transfers deterministically move a
/// non-empty prefix and the caller has to loop.
#[test]
fn short_transfers_are_injected_deterministically() {
    local_runtime().block_on(async {
        let mut config = StorageConfiguration::fast_local();
        config.short_transfer_probability = 1.0;
        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let outcome: std::io::Result<(usize, usize)> =
            run_storage_test(sim, |provider| async move {
                let file = provider
                    .open("short.db", OpenOptions::create_write().read(true))
                    .await?;
                let payload = vec![7u8; 256];
                let written = file.write_at(0, &payload).await?;

                let mut buf = vec![0u8; written];
                let read = file.read_at(0, &mut buf).await?;
                assert!(
                    buf[..read].iter().all(|byte| *byte == 7),
                    "the prefix that was transferred must be the bytes written"
                );
                Ok((written, read))
            })
            .await;

        let (written, read) = outcome.expect("short transfer test failed");
        assert!(
            written < 256 && written > 0,
            "a short write must move a non-empty prefix, moved {written}"
        );
        assert!(
            read < written.max(2) && read > 0,
            "a short read must move a non-empty prefix, moved {read}"
        );
    });
}

/// Open `path` for direct I/O the way a journal or pager does.
///
/// `DirectIo::Required` opens a file that already exists — it never creates
/// one, because creating a file and guaranteeing direct I/O on it are two
/// operations and publishing a pathname is the caller's protocol. So the file
/// is bootstrapped with an ordinary open first, then reopened with the
/// capability required.
async fn open_direct<P: StorageProvider>(
    provider: &P,
    path: &str,
    options: OpenOptions,
) -> std::io::Result<P::File> {
    let created = provider.open(path, OpenOptions::create_write()).await?;
    created.sync_all().await?;
    drop(created);
    provider
        .open(path, options.direct_io(DirectIo::Required))
        .await
}

/// Direct I/O is a property of the *open*, and it constrains offsets,
/// transfer lengths, and buffer addresses independently.
#[test]
fn direct_io_validates_alignment() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            let file = open_direct(&provider, "direct.db", OpenOptions::read_write()).await?;
            assert!(file.is_direct_io());
            let constraints = file.constraints();
            assert_eq!(constraints.offset_alignment(), 4096);
            assert!(!constraints.is_unconstrained());

            let block = constraints.offset_alignment();
            let mut aligned = AlignedBuf::for_constraints(4096, constraints);
            aligned.as_mut_slice().fill(0xAB);
            assert_eq!(file.write_at(block, aligned.as_slice()).await?, 4096);

            // Misaligned offset, misaligned length, misaligned memory: three
            // separate rejections, all InvalidInput.
            let misaligned_offset = file.write_at(block + 1, aligned.as_slice()).await;
            assert_eq!(
                misaligned_offset.expect_err("misaligned offset").kind(),
                std::io::ErrorKind::InvalidInput
            );

            let short = vec![0u8; 100];
            assert_eq!(
                file.write_at(block, &short)
                    .await
                    .expect_err("short")
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );

            let unaligned_memory = &aligned.as_slice()[8..8 + 4088];
            assert_eq!(
                file.write_at(block, unaligned_memory)
                    .await
                    .expect_err("unaligned memory")
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );

            // An aligned read gets the aligned bytes back.
            let mut read_buf = AlignedBuf::for_constraints(4096, constraints);
            assert_eq!(file.read_at(block, read_buf.as_mut_slice()).await?, 4096);
            assert!(read_buf.iter().all(|byte| *byte == 0xAB));
            Ok(())
        })
        .await;
        result.expect("direct I/O test failed");
    });
}

/// `DirectIo::Required` fails the open on a disk that cannot provide it, and
/// is never silently downgraded.
#[test]
fn direct_io_required_fails_when_unsupported() {
    local_runtime().block_on(async {
        let mut config = StorageConfiguration::fast_local();
        config.direct_io_supported = false;
        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let error = run_storage_test(sim, |provider| async move {
            provider
                .open(
                    "no-direct.db",
                    OpenOptions::create_write().direct_io(DirectIo::Required),
                )
                .await
                .err()
                .map(|error| error.kind())
        })
        .await;

        assert_eq!(
            error,
            Some(std::io::ErrorKind::Unsupported),
            "a required direct-I/O open must fail rather than downgrade"
        );
    });
}

/// `DirectIo::Optional` falls back to buffered I/O, and says so.
#[test]
fn direct_io_optional_falls_back() {
    local_runtime().block_on(async {
        let mut config = StorageConfiguration::fast_local();
        config.direct_io_supported = false;
        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let result: std::io::Result<()> = run_storage_test(sim, |provider| async move {
            let file = provider
                .open(
                    "fallback.db",
                    OpenOptions::create_write()
                        .read(true)
                        .direct_io(DirectIo::Optional),
                )
                .await?;
            assert!(!file.is_direct_io(), "the fallback is buffered I/O");
            assert!(file.constraints().is_unconstrained());

            // A buffered file takes any offset and any length.
            assert_eq!(file.write_at(3, b"seven!!").await?, 7);
            Ok(())
        })
        .await;
        result.expect("fallback test failed");
    });
}

/// Direct I/O is not durability: an uncached write still needs a sync.
#[test]
fn direct_io_writes_still_need_a_sync() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            let file = open_direct(&provider, "durability.db", OpenOptions::read_write()).await?;
            let constraints = file.constraints();
            let mut block = AlignedBuf::for_constraints(4096, constraints);
            block.as_mut_slice().fill(0x5A);
            file.write_at(0, block.as_slice()).await?;
            // Deliberately no sync: the bytes are visible, not durable.
            let mut check = AlignedBuf::for_constraints(4096, constraints);
            assert_eq!(file.read_at(0, check.as_mut_slice()).await?, 4096);
            assert!(check.iter().all(|byte| *byte == 0x5A));
            Ok::<_, std::io::Error>(())
        });

        step_until_done(&mut sim, handle)
            .await
            .expect("write failed");
    });
}

/// The simulated provider decides direct I/O before it touches the namespace,
/// so an open that must fail fails for its own reason — the production
/// provider's `create_new` guarantee, on the other side of the seam.
#[test]
fn direct_io_policy_does_not_weaken_create_new() {
    local_runtime().block_on(async {
        let mut config = StorageConfiguration::fast_local();
        // The interesting case is the disk that cannot do direct I/O at all:
        // an `Optional` open must still fail `create_new` rather than falling
        // back onto the existing file.
        config.direct_io_supported = false;
        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let kinds = run_storage_test(sim, |provider| async move {
            provider
                .open("exclusive.db", OpenOptions::create_new_write())
                .await
                .expect("first create_new must succeed");

            let optional = provider
                .open(
                    "exclusive.db",
                    OpenOptions::create_new_write().direct_io(DirectIo::Optional),
                )
                .await
                .err()
                .map(|error| error.kind());
            let required = provider
                .open(
                    "exclusive.db",
                    OpenOptions::create_new_write().direct_io(DirectIo::Required),
                )
                .await
                .err()
                .map(|error| error.kind());
            (optional, required)
        })
        .await;

        assert_eq!(
            kinds.0,
            Some(std::io::ErrorKind::AlreadyExists),
            "an Optional open must not fall back onto an existing file"
        );
        assert_eq!(
            kinds.1,
            Some(std::io::ErrorKind::Unsupported),
            "a Required open on a disk without direct I/O fails for that reason"
        );
    });
}

/// `Required` never creates a file in the simulator either — on a disk that
/// *does* support direct I/O, so the refusal is about the contract and not
/// about the device.
///
/// Mirroring production here is the point: if the simulator let a caller
/// create a file with `Required`, simulated database code would come to depend
/// on an open the real provider refuses.
#[test]
fn a_required_open_never_creates_a_file() {
    local_runtime().block_on(async {
        let outcome = run_storage_test(fast_sim(), |provider| async move {
            let mut refusals = Vec::new();
            for options in [
                OpenOptions::create_write().direct_io(DirectIo::Required),
                OpenOptions::create_new_write().direct_io(DirectIo::Required),
            ] {
                refusals.push(
                    provider
                        .open("bootstrap.db", options)
                        .await
                        .err()
                        .map(|error| error.kind()),
                );
            }
            let leaked = provider.exists("bootstrap.db").await?;

            // Without create at all, the ordinary answer.
            let missing = provider
                .open(
                    "bootstrap.db",
                    OpenOptions::read_only().direct_io(DirectIo::Required),
                )
                .await
                .err()
                .map(|error| error.kind());

            // The documented bootstrap, which does work.
            let file = open_direct(&provider, "bootstrap.db", OpenOptions::read_write()).await?;
            let direct = file.is_direct_io();
            Ok::<_, std::io::Error>((refusals, leaked, missing, direct))
        })
        .await
        .expect("simulated open sequence failed");

        assert_eq!(
            outcome.0,
            vec![
                Some(std::io::ErrorKind::Unsupported),
                Some(std::io::ErrorKind::Unsupported)
            ],
            "Required must refuse to create, whatever the lifecycle flags say"
        );
        assert!(!outcome.1, "a refused Required create must create nothing");
        assert_eq!(outcome.2, Some(std::io::ErrorKind::NotFound));
        assert!(outcome.3, "the bootstrapped file is opened for direct I/O");
    });
}

/// A refused `Required` open applies no lifecycle in the simulator either.
///
/// The production backend has to work for this: Linux applies `O_CREAT` and
/// `O_TRUNC` before rejecting `O_DIRECT`, so it defers the lifecycle until
/// direct I/O is secured. The simulator decides direct I/O before it touches
/// the namespace at all, which reaches the same contract from the other side —
/// and this pins it there, so neither backend can drift.
#[test]
fn a_refused_required_open_applies_no_lifecycle() {
    local_runtime().block_on(async {
        let mut config = StorageConfiguration::fast_local();
        config.direct_io_supported = false;
        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let outcome = run_storage_test(sim, |provider| async move {
            // A file the caller wants created: the refusal must leave nothing.
            let created = provider
                .open(
                    "required-create.db",
                    OpenOptions::create_new_write().direct_io(DirectIo::Required),
                )
                .await
                .err()
                .map(|error| error.kind());
            let leaked = provider.exists("required-create.db").await?;

            // A file the caller wants truncated: the refusal must leave its
            // contents alone.
            let seed = provider
                .open("required-truncate.db", OpenOptions::create_write())
                .await?;
            seed.write_at(0, b"PRECIOUS").await?;
            seed.sync_all().await?;
            drop(seed);

            let truncating = provider
                .open(
                    "required-truncate.db",
                    OpenOptions::create_write().direct_io(DirectIo::Required),
                )
                .await
                .err()
                .map(|error| error.kind());

            let survivor = provider
                .open("required-truncate.db", OpenOptions::read_only())
                .await?;
            let mut buf = vec![0u8; 8];
            let read = survivor.read_at(0, &mut buf).await?;
            Ok::<_, std::io::Error>((created, leaked, truncating, buf[..read].to_vec()))
        })
        .await
        .expect("simulated open sequence failed");

        assert_eq!(outcome.0, Some(std::io::ErrorKind::Unsupported));
        assert!(!outcome.1, "a refused create must not leave a file behind");
        assert_eq!(outcome.2, Some(std::io::ErrorKind::Unsupported));
        assert_eq!(
            outcome.3, b"PRECIOUS",
            "a refused truncating open must not destroy the contents"
        );
    });
}
