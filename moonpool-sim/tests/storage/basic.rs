//! Basic storage simulation tests.
//!
//! Tests fundamental storage operations: read, write, seek, size, truncate, sync,
//! and file management (create, delete, rename).

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration};
use std::io::SeekFrom;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// Helper to run an async storage test with proper simulation stepping.
async fn run_storage_test<F, Fut, T>(mut sim: SimWorld, f: F) -> T
where
    F: FnOnce(moonpool_sim::SimStorageProvider) -> Fut,
    Fut: std::future::Future<Output = T> + 'static,
    T: 'static,
{
    let provider = sim.storage_provider();
    let handle = tokio::task::spawn_local(f(provider));

    while !handle.is_finished() {
        while sim.pending_event_count() > 0 {
            sim.step();
        }
        tokio::task::yield_now().await;
    }

    handle.await.expect("task panicked")
}

/// Create a local tokio runtime for tests.
fn local_runtime() -> tokio::runtime::LocalRuntime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime")
}

/// Create a SimWorld with fast storage configuration.
fn fast_sim() -> SimWorld {
    let mut sim = SimWorld::new();
    sim.set_storage_config(StorageConfiguration::fast_local());
    sim
}

#[test]
fn test_create_write_read() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            // Create and write to a file
            let mut file = provider
                .open("test.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"hello world").await?;
            file.sync_all().await?;
            drop(file);

            // Re-open for reading and verify
            let mut file = provider.open("test.txt", OpenOptions::read_only()).await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            assert_eq!(&buf, b"hello world");

            Ok(())
        })
        .await;

        result.expect("test failed");
    });
}

#[test]
fn test_seek_operations() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            // Create a file with known content
            let mut file = provider
                .open("seek_test.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"0123456789").await?;
            file.sync_all().await?;
            drop(file);

            // Test seek operations
            let mut file = provider
                .open("seek_test.txt", OpenOptions::read_only())
                .await?;

            // SeekFrom::Start
            let pos = file.seek(SeekFrom::Start(5)).await?;
            assert_eq!(pos, 5);
            let mut buf = [0u8; 1];
            file.read_exact(&mut buf).await?;
            assert_eq!(&buf, b"5");

            // SeekFrom::Current
            let pos = file.seek(SeekFrom::Current(2)).await?;
            assert_eq!(pos, 8);
            file.read_exact(&mut buf).await?;
            assert_eq!(&buf, b"8");

            // SeekFrom::End
            let pos = file.seek(SeekFrom::End(-3)).await?;
            assert_eq!(pos, 7);
            file.read_exact(&mut buf).await?;
            assert_eq!(&buf, b"7");

            Ok(())
        })
        .await;

        result.expect("test failed");
    });
}

#[test]
fn test_file_size() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            // Create empty file
            let mut file = provider
                .open("size_test.txt", OpenOptions::create_write())
                .await?;
            assert_eq!(file.size().await?, 0);

            // Write data
            file.write_all(b"hello").await?;
            assert_eq!(file.size().await?, 5);

            // Write more
            file.write_all(b" world").await?;
            assert_eq!(file.size().await?, 11);

            file.sync_all().await?;
            Ok(())
        })
        .await;

        result.expect("test failed");
    });
}

#[test]
fn test_set_len() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            // Create file with content
            let mut file = provider
                .open("truncate_test.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"hello world").await?;
            assert_eq!(file.size().await?, 11);

            // Truncate
            file.set_len(5).await?;
            assert_eq!(file.size().await?, 5);

            // Extend
            file.set_len(20).await?;
            assert_eq!(file.size().await?, 20);

            file.sync_all().await?;
            drop(file);

            // Verify content after truncate/extend
            let mut file = provider
                .open("truncate_test.txt", OpenOptions::read_only())
                .await?;
            let mut buf = [0u8; 5];
            file.read_exact(&mut buf).await?;
            assert_eq!(&buf, b"hello");

            Ok(())
        })
        .await;

        result.expect("test failed");
    });
}

#[test]
fn test_sync_operations() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            let mut file = provider
                .open("sync_test.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"data to sync").await?;

            // Test sync_data (just data, not metadata)
            file.sync_data().await?;

            // Test sync_all (data + metadata)
            file.sync_all().await?;

            Ok(())
        })
        .await;

        result.expect("test failed");
    });
}

#[test]
fn test_file_not_found() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            // Try to open non-existent file for reading
            let result = provider
                .open("nonexistent.txt", OpenOptions::read_only())
                .await;
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

            Ok(())
        })
        .await;

        result.expect("test failed");
    });
}

#[test]
fn test_file_already_exists() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            // Create a file
            let file = provider
                .open("exists_test.txt", OpenOptions::create_write())
                .await?;
            drop(file);

            // Try to create it exclusively again
            let result = provider
                .open("exists_test.txt", OpenOptions::create_new_write())
                .await;
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

            Ok(())
        })
        .await;

        result.expect("test failed");
    });
}

#[test]
fn test_delete_and_rename() {
    local_runtime().block_on(async {
        let result: std::io::Result<()> = run_storage_test(fast_sim(), |provider| async move {
            // Create a file
            let mut file = provider
                .open("original.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"test content").await?;
            file.sync_all().await?;
            drop(file);

            // Verify it exists
            assert!(provider.exists("original.txt").await?);

            // Rename it
            provider.rename("original.txt", "renamed.txt").await?;

            // Verify rename worked
            assert!(!provider.exists("original.txt").await?);
            assert!(provider.exists("renamed.txt").await?);

            // Verify content is preserved
            let mut file = provider
                .open("renamed.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            assert_eq!(&buf, b"test content");
            drop(file);

            // Delete it
            provider.delete("renamed.txt").await?;
            assert!(!provider.exists("renamed.txt").await?);

            Ok(())
        })
        .await;

        result.expect("test failed");
    });
}
