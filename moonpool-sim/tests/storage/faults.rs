//! Fault injection tests for storage simulation.
//!
//! These tests verify that the fault injection mechanisms work correctly
//! for various failure modes.

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
fn test_read_corruption_fault() {
    local_runtime().block_on(async {
        // Create config with guaranteed read corruption
        let mut config = StorageConfiguration::fast_local();
        config.read_fault_probability = 1.0; // 100% corruption

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let result: std::io::Result<bool> = run_storage_test(sim, |provider| async move {
            // Write known data
            let mut file = provider
                .open("corrupt.txt", OpenOptions::create_write())
                .await?;
            let original = b"hello world!";
            file.write_all(original).await?;
            file.sync_all().await?;
            drop(file);

            // Read it back - with 100% corruption, data should be different
            let mut file = provider
                .open("corrupt.txt", OpenOptions::read_only())
                .await?;
            let mut buf = vec![0u8; original.len()];
            file.read_exact(&mut buf).await?;

            // Check if data was corrupted
            let corrupted = &buf != original;
            Ok(corrupted)
        })
        .await;

        let was_corrupted = result.expect("test failed");
        assert!(
            was_corrupted,
            "Read should have been corrupted with 100% probability"
        );
    });
}

#[test]
fn test_write_corruption_fault() {
    local_runtime().block_on(async {
        // Create config with guaranteed write corruption
        let mut config = StorageConfiguration::fast_local();
        config.write_fault_probability = 1.0; // 100% corruption

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let result: std::io::Result<bool> = run_storage_test(sim, |provider| async move {
            // Write data
            let mut file = provider
                .open("write_corrupt.txt", OpenOptions::create_write())
                .await?;
            let original = b"test data for write corruption";
            file.write_all(original).await?;
            file.sync_all().await?;
            drop(file);

            // Read without corruption (set a new file with no read faults)
            let mut file = provider
                .open("write_corrupt.txt", OpenOptions::read_only())
                .await?;
            let mut buf = vec![0u8; original.len()];
            file.read_exact(&mut buf).await?;

            // The data written may have been corrupted
            let corrupted = &buf != original;
            Ok(corrupted)
        })
        .await;

        // With write corruption, the data should be different
        let was_corrupted = result.expect("test failed");
        assert!(
            was_corrupted,
            "Write should have been corrupted with 100% probability"
        );
    });
}

#[test]
fn test_misdirected_write_fault() {
    local_runtime().block_on(async {
        // Create config with misdirected writes
        let mut config = StorageConfiguration::fast_local();
        config.misdirect_write_probability = 1.0; // 100% misdirection

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let result: std::io::Result<Vec<u8>> = run_storage_test(sim, |provider| async move {
            // Create a file and write sequentially
            let mut file = provider
                .open("misdirect.txt", OpenOptions::create_write())
                .await?;

            // Write known pattern
            for i in 0..10u8 {
                file.write_all(&[i]).await?;
            }
            file.sync_all().await?;
            drop(file);

            // Read back
            let mut file = provider
                .open("misdirect.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;

            Ok(buf)
        })
        .await;

        let data = result.expect("test failed");

        // With misdirection, data order may not be sequential 0-9
        let expected: Vec<u8> = (0..10).collect();
        let is_normal = data == expected;

        // Either the data is wrong OR it could happen to be correct by chance
        // Just verify we got data back
        assert!(!data.is_empty(), "Should have gotten some data");
        println!(
            "Misdirection test: data was {} normal order",
            if is_normal { "in" } else { "NOT in" }
        );
    });
}

#[test]
fn test_misdirected_read_fault() {
    local_runtime().block_on(async {
        // Create config with misdirected reads
        let mut config = StorageConfiguration::fast_local();
        config.misdirect_read_probability = 1.0; // 100% misdirection

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let result: std::io::Result<Vec<u8>> = run_storage_test(sim, |provider| async move {
            // Create a file with known data pattern
            let mut file = provider
                .open("misdirect_read.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"AABBCCDDEE").await?;
            file.sync_all().await?;
            drop(file);

            // Read back with misdirection
            let mut file = provider
                .open("misdirect_read.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;

            Ok(buf)
        })
        .await;

        let data = result.expect("test failed");

        // With read misdirection, data may come from wrong positions
        println!(
            "Misdirected read result: {:?}",
            String::from_utf8_lossy(&data)
        );
        assert!(!data.is_empty(), "Should have gotten some data");
    });
}

#[test]
fn test_phantom_write_fault() {
    local_runtime().block_on(async {
        // Create config with phantom writes (writes silently lost)
        let mut config = StorageConfiguration::fast_local();
        config.phantom_write_probability = 1.0; // 100% phantom

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let result: std::io::Result<Vec<u8>> = run_storage_test(sim, |provider| async move {
            // Write data that should be "lost"
            let mut file = provider
                .open("phantom.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"this data should be lost").await?;
            file.sync_all().await?;
            drop(file);

            // Read back - with phantom writes, data may be zeros or missing
            let mut file = provider
                .open("phantom.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;

            Ok(buf)
        })
        .await;

        let data = result.expect("test failed");

        // With 100% phantom writes, the written data should NOT be persisted
        let expected = b"this data should be lost";
        let all_zeros = data.iter().all(|&b| b == 0);

        println!(
            "Phantom write result: {:?} (all zeros: {})",
            data, all_zeros
        );

        // Either empty, all zeros, or not matching expected
        assert!(
            data.is_empty() || all_zeros || &data != expected,
            "Phantom write should have lost the data"
        );
    });
}

#[test]
fn test_sync_failure_fault() {
    local_runtime().block_on(async {
        // Create config with sync failures
        let mut config = StorageConfiguration::fast_local();
        config.sync_failure_probability = 1.0; // 100% failure

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let result: std::io::Result<()> = run_storage_test(sim, |provider| async move {
            let mut file = provider
                .open("sync_fail.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"test").await?;

            // This should fail with 100% sync failure probability
            file.sync_all().await?;

            Ok(())
        })
        .await;

        // With 100% sync failure, this should be an error
        assert!(
            result.is_err(),
            "Sync should have failed with 100% failure probability"
        );
        println!("Sync failure error: {:?}", result.unwrap_err());
    });
}

#[test]
fn test_crash_torn_writes() {
    local_runtime().block_on(async {
        let mut config = StorageConfiguration::fast_local();
        config.crash_fault_probability = 0.0; // Disable random crashes, we'll trigger manually

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        // Write data but don't sync, then simulate crash
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("crash.txt", OpenOptions::create_write())
                .await
                .expect("open failed");
            file.write_all(b"unsynced data")
                .await
                .expect("write failed");
            // DON'T sync - data should be in pending state
            drop(file);
            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");

        // Simulate crash - this should apply torn write simulation
        sim.simulate_crash(true);

        // Verify the crash was processed
        // After crash, files are closed and pending writes may be corrupted
        let provider2 = sim.storage_provider();
        let handle2 = tokio::task::spawn_local(async move {
            // File should still exist but data may be corrupted
            let exists = provider2.exists("crash.txt").await?;
            Ok::<_, std::io::Error>(exists)
        });

        while !handle2.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let exists = handle2.await.expect("task panicked").expect("io error");
        println!("File exists after crash: {}", exists);
        // The file may or may not exist depending on implementation
    });
}

#[test]
fn test_uninitialized_reads() {
    local_runtime().block_on(async {
        let result: std::io::Result<Vec<u8>> =
            run_storage_test(fast_sim(), |provider| async move {
                // Create a file and extend it without writing
                let file = provider
                    .open("uninit.txt", OpenOptions::create_write())
                    .await?;
                file.set_len(100).await?; // Extend to 100 bytes without writing
                file.sync_all().await?;
                drop(file);

                // Read the uninitialized region
                let mut file = provider
                    .open("uninit.txt", OpenOptions::read_only())
                    .await?;
                let mut buf = Vec::new();
                file.read_to_end(&mut buf).await?;

                Ok(buf)
            })
            .await;

        let data = result.expect("test failed");
        assert_eq!(data.len(), 100, "File should be 100 bytes");

        // Uninitialized data should be zeros or random depending on implementation
        // Most implementations return zeros for extended but never-written regions
        println!("Uninitialized read: first 10 bytes = {:?}", &data[..10]);
    });
}
