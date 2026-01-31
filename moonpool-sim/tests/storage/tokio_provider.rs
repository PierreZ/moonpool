//! Tests for TokioStorageProvider - the real filesystem implementation.
//!
//! These tests verify that the StorageProvider trait works correctly with
//! actual filesystem operations, following the same pattern as network/traits.rs
//! for TokioNetworkProvider.

use moonpool_core::{OpenOptions, StorageFile, StorageProvider, TokioStorageProvider};
use std::io::SeekFrom;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// Helper to create a local runtime for tests.
fn local_runtime() -> tokio::runtime::LocalRuntime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime")
}

#[test]
fn test_tokio_provider_basic_write_read() {
    local_runtime().block_on(async move {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("test.txt");
        let file_path_str = file_path.to_str().unwrap();

        let provider = TokioStorageProvider::new();

        // Create and write
        let mut file = provider
            .open(file_path_str, OpenOptions::create_write())
            .await
            .expect("Failed to create file");

        let data = b"Hello, TokioStorageProvider!";
        file.write_all(data).await.expect("Failed to write");
        file.sync_all().await.expect("Failed to sync");
        drop(file);

        // Read back
        let mut file = provider
            .open(file_path_str, OpenOptions::read_only())
            .await
            .expect("Failed to open for read");

        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await.expect("Failed to read");

        assert_eq!(&buf, data);
    });
}

#[test]
fn test_tokio_provider_exists() {
    local_runtime().block_on(async move {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("exists_test.txt");
        let file_path_str = file_path.to_str().unwrap();

        let provider = TokioStorageProvider::new();

        // File should not exist initially
        assert!(
            !provider.exists(file_path_str).await.expect("exists failed"),
            "File should not exist initially"
        );

        // Create the file
        let file = provider
            .open(file_path_str, OpenOptions::create_write())
            .await
            .expect("Failed to create file");
        drop(file);

        // Now it should exist
        assert!(
            provider.exists(file_path_str).await.expect("exists failed"),
            "File should exist after creation"
        );
    });
}

#[test]
fn test_tokio_provider_delete() {
    local_runtime().block_on(async move {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("delete_test.txt");
        let file_path_str = file_path.to_str().unwrap();

        let provider = TokioStorageProvider::new();

        // Create the file
        let file = provider
            .open(file_path_str, OpenOptions::create_write())
            .await
            .expect("Failed to create file");
        drop(file);

        assert!(provider.exists(file_path_str).await.expect("exists failed"));

        // Delete the file
        provider.delete(file_path_str).await.expect("delete failed");

        // Should no longer exist
        assert!(
            !provider.exists(file_path_str).await.expect("exists failed"),
            "File should not exist after deletion"
        );
    });
}

#[test]
fn test_tokio_provider_rename() {
    local_runtime().block_on(async move {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let old_path = temp_dir.path().join("old_name.txt");
        let new_path = temp_dir.path().join("new_name.txt");
        let old_path_str = old_path.to_str().unwrap();
        let new_path_str = new_path.to_str().unwrap();

        let provider = TokioStorageProvider::new();

        // Create file with data
        let mut file = provider
            .open(old_path_str, OpenOptions::create_write())
            .await
            .expect("Failed to create file");
        file.write_all(b"rename test data")
            .await
            .expect("Failed to write");
        drop(file);

        // Rename
        provider
            .rename(old_path_str, new_path_str)
            .await
            .expect("rename failed");

        // Old path should not exist, new path should
        assert!(
            !provider.exists(old_path_str).await.expect("exists failed"),
            "Old path should not exist"
        );
        assert!(
            provider.exists(new_path_str).await.expect("exists failed"),
            "New path should exist"
        );

        // Verify data is preserved
        let mut file = provider
            .open(new_path_str, OpenOptions::read_only())
            .await
            .expect("Failed to open renamed file");
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await.expect("Failed to read");
        assert_eq!(&buf, b"rename test data");
    });
}

#[test]
fn test_tokio_provider_seek_operations() {
    local_runtime().block_on(async move {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("seek_test.txt");
        let file_path_str = file_path.to_str().unwrap();

        let provider = TokioStorageProvider::new();

        // Create file with known content
        let mut file = provider
            .open(file_path_str, OpenOptions::create_write())
            .await
            .expect("Failed to create file");
        file.write_all(b"0123456789")
            .await
            .expect("Failed to write");
        drop(file);

        // Open for reading and test seeks
        let mut file = provider
            .open(file_path_str, OpenOptions::read_only())
            .await
            .expect("Failed to open for read");

        // SeekFrom::Start
        let pos = file.seek(SeekFrom::Start(5)).await.expect("seek failed");
        assert_eq!(pos, 5);
        let mut buf = [0u8; 1];
        file.read_exact(&mut buf).await.expect("read failed");
        assert_eq!(&buf, b"5");

        // SeekFrom::Current
        let pos = file.seek(SeekFrom::Current(2)).await.expect("seek failed");
        assert_eq!(pos, 8); // Was at 6, +2 = 8
        file.read_exact(&mut buf).await.expect("read failed");
        assert_eq!(&buf, b"8");

        // SeekFrom::End
        let pos = file.seek(SeekFrom::End(-2)).await.expect("seek failed");
        assert_eq!(pos, 8); // 10 - 2 = 8
        file.read_exact(&mut buf).await.expect("read failed");
        assert_eq!(&buf, b"8");
    });
}

#[test]
fn test_tokio_provider_set_len_truncate() {
    local_runtime().block_on(async move {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("truncate_test.txt");
        let file_path_str = file_path.to_str().unwrap();

        let provider = TokioStorageProvider::new();

        // Create file with content
        let mut file = provider
            .open(file_path_str, OpenOptions::create_write())
            .await
            .expect("Failed to create file");
        file.write_all(b"0123456789")
            .await
            .expect("Failed to write");
        file.sync_all().await.expect("Failed to sync");

        // Verify size
        let size = file.size().await.expect("size failed");
        assert_eq!(size, 10);

        // Truncate to 5 bytes
        file.set_len(5).await.expect("set_len failed");
        let size = file.size().await.expect("size failed");
        assert_eq!(size, 5);
        drop(file);

        // Verify content is truncated
        let mut file = provider
            .open(file_path_str, OpenOptions::read_only())
            .await
            .expect("Failed to open");
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await.expect("read failed");
        assert_eq!(&buf, b"01234");
    });
}

#[test]
fn test_tokio_provider_set_len_extend() {
    local_runtime().block_on(async move {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("extend_test.txt");
        let file_path_str = file_path.to_str().unwrap();

        let provider = TokioStorageProvider::new();

        // Create file with content
        let mut file = provider
            .open(file_path_str, OpenOptions::create_write())
            .await
            .expect("Failed to create file");
        file.write_all(b"hello").await.expect("Failed to write");
        file.sync_all().await.expect("Failed to sync");

        // Extend to 10 bytes
        file.set_len(10).await.expect("set_len failed");
        let size = file.size().await.expect("size failed");
        assert_eq!(size, 10);
        drop(file);

        // Verify extended content (should have zeros in extended region)
        let mut file = provider
            .open(file_path_str, OpenOptions::read_only())
            .await
            .expect("Failed to open");
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await.expect("read failed");
        assert_eq!(buf.len(), 10);
        assert_eq!(&buf[..5], b"hello");
        // Extended region should be zeros
        assert!(buf[5..].iter().all(|&b| b == 0));
    });
}

#[test]
fn test_tokio_provider_sync_data() {
    local_runtime().block_on(async move {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("sync_data_test.txt");
        let file_path_str = file_path.to_str().unwrap();

        let provider = TokioStorageProvider::new();

        let mut file = provider
            .open(file_path_str, OpenOptions::create_write())
            .await
            .expect("Failed to create file");
        file.write_all(b"sync data test")
            .await
            .expect("Failed to write");

        // sync_data should succeed
        file.sync_data().await.expect("sync_data failed");
        drop(file);

        // Verify data is persisted
        let mut file = provider
            .open(file_path_str, OpenOptions::read_only())
            .await
            .expect("Failed to open");
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await.expect("read failed");
        assert_eq!(&buf, b"sync data test");
    });
}

#[test]
fn test_tokio_provider_error_not_found() {
    local_runtime().block_on(async move {
        let provider = TokioStorageProvider::new();

        // Try to open non-existent file for reading
        let result = provider
            .open("/nonexistent/path/to/file.txt", OpenOptions::read_only())
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    });
}

#[test]
fn test_tokio_provider_error_already_exists() {
    local_runtime().block_on(async move {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("exists.txt");
        let file_path_str = file_path.to_str().unwrap();

        let provider = TokioStorageProvider::new();

        // Create the file first
        let file = provider
            .open(file_path_str, OpenOptions::create_write())
            .await
            .expect("Failed to create file");
        drop(file);

        // Try to create_new (should fail because file exists)
        let result = provider
            .open(file_path_str, OpenOptions::create_new_write())
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    });
}

#[test]
fn test_tokio_provider_clone() {
    local_runtime().block_on(async move {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("clone_test.txt");
        let file_path_str = file_path.to_str().unwrap();

        let provider = TokioStorageProvider::new();
        let cloned_provider = provider.clone();

        // Create with original provider
        let mut file = provider
            .open(file_path_str, OpenOptions::create_write())
            .await
            .expect("Failed to create file");
        file.write_all(b"original").await.expect("Failed to write");
        drop(file);

        // Read with cloned provider
        let mut file = cloned_provider
            .open(file_path_str, OpenOptions::read_only())
            .await
            .expect("Failed to open with cloned provider");
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await.expect("read failed");
        assert_eq!(&buf, b"original");
    });
}

#[test]
fn test_tokio_provider_default() {
    local_runtime().block_on(async move {
        // Test that TokioStorageProvider implements Default
        let provider = TokioStorageProvider::default();

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("default_test.txt");
        let file_path_str = file_path.to_str().unwrap();

        let file = provider
            .open(file_path_str, OpenOptions::create_write())
            .await
            .expect("Failed to create file with default provider");
        drop(file);

        assert!(provider.exists(file_path_str).await.expect("exists failed"));
    });
}

/// Generic function that works with any StorageProvider - demonstrates trait polymorphism
async fn write_and_read_generic<P: StorageProvider>(
    provider: P,
    path: &str,
    data: &[u8],
) -> std::io::Result<Vec<u8>> {
    // Write
    let mut file = provider.open(path, OpenOptions::create_write()).await?;
    file.write_all(data).await?;
    file.sync_all().await?;
    drop(file);

    // Read
    let mut file = provider.open(path, OpenOptions::read_only()).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    Ok(buf)
}

#[test]
fn test_tokio_provider_generic_trait_usage() {
    local_runtime().block_on(async move {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("generic_test.txt");
        let file_path_str = file_path.to_str().unwrap();

        let provider = TokioStorageProvider::new();
        let data = b"Generic trait test!";

        let result = write_and_read_generic(provider, file_path_str, data)
            .await
            .expect("Generic operation failed");

        assert_eq!(&result, data);
    });
}
