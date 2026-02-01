//! Crash recovery scenario tests for storage simulation.
//!
//! These tests verify proper behavior of data durability guarantees
//! across crash scenarios, following patterns from TigerBeetle and
//! FoundationDB's crash consistency testing.

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

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

/// Helper to run storage operations with simulation stepping.
async fn step_until_done<T: 'static>(
    sim: &mut SimWorld,
    handle: tokio::task::JoinHandle<std::io::Result<T>>,
) -> std::io::Result<T> {
    while !handle.is_finished() {
        while sim.pending_event_count() > 0 {
            sim.step();
        }
        tokio::task::yield_now().await;
    }
    handle.await.expect("task panicked")
}

/// Test: Write, sync, crash, recover - data should be intact
#[test]
fn test_write_sync_crash_recovery() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        let data = b"This data was properly synced before crash";

        // Write and sync data
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("synced_recovery.txt", OpenOptions::create_write())
                .await?;
            file.write_all(data).await?;
            file.sync_all().await?;
            drop(file);
            Ok::<_, std::io::Error>(())
        });
        step_until_done(&mut sim, handle)
            .await
            .expect("write failed");

        // Crash the system
        sim.simulate_crash(true);

        // Recover and verify data is intact
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("synced_recovery.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            Ok::<_, std::io::Error>(buf)
        });

        let recovered = step_until_done(&mut sim, handle)
            .await
            .expect("read failed");
        assert_eq!(&recovered, data, "Synced data should survive crash intact");
    });
}

/// Test: Write without sync, crash - data may be lost
#[test]
fn test_write_nosync_crash_recovery() {
    local_runtime().block_on(async {
        let mut config = StorageConfiguration::fast_local();
        config.crash_fault_probability = 1.0; // 100% corruption on crash

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let original_data = b"This data was NOT synced before crash";

        // Write WITHOUT sync
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("unsynced_recovery.txt", OpenOptions::create_write())
                .await?;
            file.write_all(original_data).await?;
            // NO sync_all() here!
            drop(file);
            Ok::<_, std::io::Error>(())
        });
        step_until_done(&mut sim, handle)
            .await
            .expect("write failed");

        // Crash
        sim.simulate_crash(true);

        // Try to recover
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            if !provider.exists("unsynced_recovery.txt").await? {
                return Ok::<_, std::io::Error>(None);
            }
            let mut file = provider
                .open("unsynced_recovery.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            Ok(Some(buf))
        });

        let recovered = step_until_done(&mut sim, handle)
            .await
            .expect("read failed");

        // With 100% crash fault, unsynced data should be affected
        match recovered {
            None => println!("File was lost after crash (expected behavior)"),
            Some(data) if data.is_empty() => println!("File is empty after crash"),
            Some(data) if data != original_data => println!("Data was corrupted after crash"),
            Some(_) => println!("Data survived (pending writes may have been flushed)"),
        }
    });
}

/// Test: Multiple writes, partial sync, crash
#[test]
fn test_partial_sync_crash_recovery() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Write two pieces of data, only sync the first
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("partial_sync.txt", OpenOptions::create_write())
                .await?;

            // First write - synced
            file.write_all(b"SYNCED_DATA|").await?;
            file.sync_all().await?;

            // Second write - NOT synced
            file.write_all(b"UNSYNCED_DATA").await?;
            // No sync here!

            drop(file);
            Ok::<_, std::io::Error>(())
        });
        step_until_done(&mut sim, handle)
            .await
            .expect("write failed");

        // Crash
        sim.simulate_crash(true);

        // Recover
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("partial_sync.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            Ok::<_, std::io::Error>(buf)
        });

        let recovered = step_until_done(&mut sim, handle)
            .await
            .expect("read failed");
        let content = String::from_utf8_lossy(&recovered);

        println!("Recovered content: {:?}", content);

        // First synced part should be present
        assert!(
            content.starts_with("SYNCED_DATA|"),
            "Synced portion should be recovered"
        );
    });
}

/// Test: Append pattern - multiple syncs
#[test]
fn test_append_multiple_syncs_crash_recovery() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Append and sync multiple times
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("append.txt", OpenOptions::create_write())
                .await?;

            for i in 1..=5 {
                file.write_all(format!("Entry {}\n", i).as_bytes()).await?;
                file.sync_all().await?;
            }

            drop(file);
            Ok::<_, std::io::Error>(())
        });
        step_until_done(&mut sim, handle)
            .await
            .expect("write failed");

        // Crash
        sim.simulate_crash(true);

        // Recover
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("append.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            Ok::<_, std::io::Error>(buf)
        });

        let recovered = step_until_done(&mut sim, handle)
            .await
            .expect("read failed");
        let content = String::from_utf8_lossy(&recovered);

        println!("Recovered append log:\n{}", content);

        // All synced entries should be present
        for i in 1..=5 {
            assert!(
                content.contains(&format!("Entry {}", i)),
                "Entry {} should be recovered",
                i
            );
        }
    });
}

/// Test: Overwrite existing data, crash before sync
#[test]
fn test_overwrite_crash_recovery() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Create file with initial synced content
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("overwrite.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"ORIGINAL_CONTENT").await?;
            file.sync_all().await?;
            drop(file);
            Ok::<_, std::io::Error>(())
        });
        step_until_done(&mut sim, handle)
            .await
            .expect("initial write failed");

        // Overwrite without sync
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open(
                    "overwrite.txt",
                    OpenOptions::new().write(true).truncate(true),
                )
                .await?;
            file.write_all(b"NEW_CONTENT").await?;
            // NO sync!
            drop(file);
            Ok::<_, std::io::Error>(())
        });
        step_until_done(&mut sim, handle)
            .await
            .expect("overwrite failed");

        // Crash
        sim.simulate_crash(true);

        // Recover - should see some content
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("overwrite.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            Ok::<_, std::io::Error>(buf)
        });

        let recovered = step_until_done(&mut sim, handle)
            .await
            .expect("read failed");
        let content = String::from_utf8_lossy(&recovered);

        println!("After overwrite crash: {:?}", content);
        // Content could be original, new, or corrupted depending on implementation
    });
}

/// Test: Rename after write, crash
#[test]
fn test_rename_crash_recovery() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Write to temp file, sync, rename to final
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            // Write to temp
            let mut file = provider
                .open("data.tmp", OpenOptions::create_write())
                .await?;
            file.write_all(b"Important data").await?;
            file.sync_all().await?;
            drop(file);

            // Rename to final (atomic on most filesystems)
            provider.rename("data.tmp", "data.final").await?;

            Ok::<_, std::io::Error>(())
        });
        step_until_done(&mut sim, handle)
            .await
            .expect("write/rename failed");

        // Crash
        sim.simulate_crash(true);

        // Verify final file exists
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let temp_exists = provider.exists("data.tmp").await?;
            let final_exists = provider.exists("data.final").await?;
            Ok::<_, std::io::Error>((temp_exists, final_exists))
        });

        let (temp_exists, final_exists) = step_until_done(&mut sim, handle)
            .await
            .expect("exists check failed");

        println!(
            "After rename crash: temp={}, final={}",
            temp_exists, final_exists
        );

        // After successful rename + crash, final should exist, temp should not
        assert!(final_exists, "Renamed file should exist after crash");
        assert!(!temp_exists, "Temp file should not exist after rename");
    });
}

/// Test: Delete file, crash - file should stay deleted
#[test]
fn test_delete_crash_recovery() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Create and sync file
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("to_delete.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"delete me").await?;
            file.sync_all().await?;
            drop(file);

            // Delete the file
            provider.delete("to_delete.txt").await?;

            Ok::<_, std::io::Error>(())
        });
        step_until_done(&mut sim, handle)
            .await
            .expect("delete failed");

        // Crash
        sim.simulate_crash(true);

        // Verify file is still deleted
        let provider = sim.storage_provider();
        let handle =
            tokio::task::spawn_local(async move { provider.exists("to_delete.txt").await });

        let exists = step_until_done(&mut sim, handle)
            .await
            .expect("exists check failed");
        assert!(!exists, "Deleted file should stay deleted after crash");
    });
}

/// Test: Create multiple files, crash mid-creation
#[test]
fn test_multiple_files_crash_recovery() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Create files, syncing some but not all
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            // File 1 - fully synced
            let mut file = provider
                .open("file1.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"File 1 content").await?;
            file.sync_all().await?;
            drop(file);

            // File 2 - fully synced
            let mut file = provider
                .open("file2.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"File 2 content").await?;
            file.sync_all().await?;
            drop(file);

            // File 3 - NOT synced
            let mut file = provider
                .open("file3.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"File 3 content").await?;
            // NO sync!
            drop(file);

            Ok::<_, std::io::Error>(())
        });
        step_until_done(&mut sim, handle)
            .await
            .expect("create failed");

        // Crash
        sim.simulate_crash(true);

        // Check which files survived
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let f1 = provider.exists("file1.txt").await?;
            let f2 = provider.exists("file2.txt").await?;
            let f3 = provider.exists("file3.txt").await?;
            Ok::<_, std::io::Error>((f1, f2, f3))
        });

        let (f1, f2, f3) = step_until_done(&mut sim, handle)
            .await
            .expect("exists check failed");

        println!("After crash: file1={}, file2={}, file3={}", f1, f2, f3);

        // Synced files should definitely exist
        assert!(f1, "Synced file1 should exist");
        assert!(f2, "Synced file2 should exist");
        // Unsynced file3 may or may not exist
    });
}

/// Test: Seek and overwrite specific region, crash
#[test]
fn test_seek_overwrite_crash_recovery() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Create file with known content
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("seek_test.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"AAAAAAAAAA").await?; // 10 A's
            file.sync_all().await?;
            drop(file);

            // Reopen, seek, overwrite middle
            let mut file = provider
                .open("seek_test.txt", OpenOptions::new().read(true).write(true))
                .await?;
            file.seek(std::io::SeekFrom::Start(4)).await?;
            file.write_all(b"BB").await?;
            file.sync_all().await?; // Sync this change
            drop(file);

            Ok::<_, std::io::Error>(())
        });
        step_until_done(&mut sim, handle)
            .await
            .expect("seek/write failed");

        // Crash
        sim.simulate_crash(true);

        // Verify the change persisted
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("seek_test.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            Ok::<_, std::io::Error>(buf)
        });

        let recovered = step_until_done(&mut sim, handle)
            .await
            .expect("read failed");

        println!(
            "After seek/overwrite crash: {:?}",
            String::from_utf8_lossy(&recovered)
        );

        // Should see "AAAABBAAAA"
        assert_eq!(&recovered, b"AAAABBAAAA", "Synced overwrite should persist");
    });
}

/// Test: Extend file with set_len, crash
#[test]
fn test_set_len_crash_recovery() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Create file, extend it, sync
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let file = provider
                .open("extended.txt", OpenOptions::create_write())
                .await?;
            file.set_len(1000).await?;
            file.sync_all().await?;
            drop(file);
            Ok::<_, std::io::Error>(())
        });
        step_until_done(&mut sim, handle)
            .await
            .expect("extend failed");

        // Crash
        sim.simulate_crash(true);

        // Verify size persisted
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let file = provider
                .open("extended.txt", OpenOptions::read_only())
                .await?;
            let size = file.size().await?;
            Ok::<_, std::io::Error>(size)
        });

        let size = step_until_done(&mut sim, handle)
            .await
            .expect("size check failed");

        println!("Extended file size after crash: {}", size);
        assert_eq!(size, 1000, "Extended size should persist after crash");
    });
}

/// Test: Truncate file with set_len, crash
#[test]
fn test_truncate_crash_recovery() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Create large file
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("truncate.txt", OpenOptions::create_write())
                .await?;
            file.write_all(&vec![b'x'; 1000]).await?;
            file.sync_all().await?;

            // Truncate to 100 bytes
            file.set_len(100).await?;
            file.sync_all().await?;
            drop(file);
            Ok::<_, std::io::Error>(())
        });
        step_until_done(&mut sim, handle)
            .await
            .expect("truncate failed");

        // Crash
        sim.simulate_crash(true);

        // Verify truncated size
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let file = provider
                .open("truncate.txt", OpenOptions::read_only())
                .await?;
            let size = file.size().await?;
            Ok::<_, std::io::Error>(size)
        });

        let size = step_until_done(&mut sim, handle)
            .await
            .expect("size check failed");

        println!("Truncated file size after crash: {}", size);
        assert_eq!(size, 100, "Truncated size should persist after crash");
    });
}
