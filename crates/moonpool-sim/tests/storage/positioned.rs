//! Positioned file I/O: `read_at` / `write_at`.
//!
//! These are the Phase-1 acceptance tests for the primitive a database
//! actually wants — read and write at an explicit offset, with no shared seek
//! cursor and no pretence of being `read_exact`/`write_all`.

use futures::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration};
use std::io::SeekFrom;
use std::net::IpAddr;

fn test_ip() -> IpAddr {
    "127.0.0.1".parse().expect("valid IP")
}

fn local_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime")
}

fn fast_sim() -> SimWorld {
    let mut sim = SimWorld::new();
    sim.set_storage_config(StorageConfiguration::fast_local());
    sim
}

/// Run one storage scenario, stepping the world until the task finishes.
async fn run_storage_test<F, Fut, T>(mut sim: SimWorld, f: F) -> T
where
    F: FnOnce(moonpool_sim::SimStorageProvider) -> Fut,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let provider = sim.storage_provider(test_ip());
    let handle = tokio::spawn(f(provider));

    while !handle.is_finished() {
        while sim.pending_event_count() > 0 {
            sim.step();
        }
        tokio::task::yield_now().await;
    }

    handle.await.expect("task panicked")
}

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

/// Positioned operations must not move the stream cursor, in either direction.
#[test]
fn positioned_operations_leave_the_stream_cursor_alone() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            let mut file = provider
                .open("cursor.db", OpenOptions::create_write().read(true))
                .await?;
            file.write_all(b"0123456789").await?;
            let before = file.seek(SeekFrom::Start(3)).await?;
            assert_eq!(before, 3);

            file.write_at(6, b"XY").await?;
            let mut buf = [0u8; 2];
            file.read_at(0, &mut buf).await?;

            // The cursor is still where the seek left it, so a stream read
            // starts at byte 3.
            let after = file.seek(SeekFrom::Current(0)).await?;
            assert_eq!(after, 3, "positioned I/O moved the stream cursor");

            let mut rest = vec![0u8; 3];
            file.read_exact(&mut rest).await?;
            assert_eq!(&rest, b"345");
            Ok(())
        })
        .await;
        result.expect("cursor test failed");
    });
}

/// One file image: a positioned write is visible to a stream read, and a
/// stream write is visible to a positioned read.
#[test]
fn positioned_and_stream_io_share_one_file_image() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            let mut file = provider
                .open("shared.db", OpenOptions::create_write().read(true))
                .await?;

            // Stream write, positioned read.
            file.write_all(b"streamed").await?;
            let mut buf = vec![0u8; 8];
            assert_eq!(file.read_at(0, &mut buf).await?, 8);
            assert_eq!(&buf, b"streamed");

            // Positioned write, stream read.
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

/// Positioned writes ignore append mode: they land where they are told.
#[test]
fn positioned_writes_ignore_append_mode() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            let mut file = provider
                .open(
                    "append.db",
                    OpenOptions::new()
                        .write(true)
                        .create(true)
                        .append(true)
                        .read(true),
                )
                .await?;
            file.write_all(b"tail").await?;
            file.write_at(0, b"HEAD").await?;

            let mut buf = vec![0u8; 4];
            assert_eq!(file.read_at(0, &mut buf).await?, 4);
            assert_eq!(&buf, b"HEAD");
            assert_eq!(
                file.size().await?,
                4,
                "the positioned write overwrote in place"
            );
            Ok(())
        })
        .await;
        result.expect("append test failed");
    });
}
