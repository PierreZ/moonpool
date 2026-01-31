//! Direct API tests for storage crash simulation.
//!
//! These tests verify the `sim.simulate_crash()` API directly,
//! following the same pattern as `network/partition.rs` tests
//! for `sim.partition_pair()`.

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

/// Test the basic simulate_crash API call
#[test]
fn test_simulate_crash_api_basic() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Create a file
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let file = provider
                .open("test.txt", OpenOptions::create_write())
                .await?;
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

        // Call simulate_crash - should not panic
        sim.simulate_crash(true);

        // API should be callable multiple times
        sim.simulate_crash(false);
        sim.simulate_crash(true);
    });
}

/// Test that synced data survives a crash
#[test]
fn test_simulate_crash_synced_data_survives() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        let data = b"This data is synced and should survive crash!";

        // Write and sync data
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("synced.txt", OpenOptions::create_write())
                .await?;
            file.write_all(data).await?;
            file.sync_all().await?;
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

        // Simulate crash
        sim.simulate_crash(true);

        // Read back - synced data should be intact
        let provider2 = sim.storage_provider();
        let handle2 = tokio::task::spawn_local(async move {
            let mut file = provider2
                .open("synced.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            Ok::<_, std::io::Error>(buf)
        });

        while !handle2.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let read_data = handle2.await.expect("task panicked").expect("io error");
        assert_eq!(&read_data, data, "Synced data should survive crash intact");
    });
}

/// Test that unsynced data may be lost after crash
#[test]
fn test_simulate_crash_unsynced_data_behavior() {
    local_runtime().block_on(async {
        let mut config = StorageConfiguration::fast_local();
        config.crash_fault_probability = 1.0; // 100% crash corruption

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let original_data = b"This data is NOT synced";

        // Write data without syncing
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("unsynced.txt", OpenOptions::create_write())
                .await?;
            file.write_all(original_data).await?;
            // NO sync_all() here!
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

        // Simulate crash with high corruption probability
        sim.simulate_crash(true);

        // Read back - data may be corrupted or lost
        let provider2 = sim.storage_provider();
        let handle2 = tokio::task::spawn_local(async move {
            let exists = provider2.exists("unsynced.txt").await?;
            if !exists {
                return Ok::<_, std::io::Error>(None);
            }

            let mut file = provider2
                .open("unsynced.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            Ok(Some(buf))
        });

        while !handle2.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let read_result = handle2.await.expect("task panicked").expect("io error");

        // With 100% crash fault probability, data should be affected
        // Either missing, empty, or different from original
        match read_result {
            None => {
                println!("File doesn't exist after crash (expected with high crash probability)")
            }
            Some(data) if data.is_empty() => println!("File is empty after crash"),
            Some(data) if data != original_data => {
                println!("Data corrupted after crash (expected)")
            }
            Some(data) => println!(
                "Data survived crash (can happen if pending writes were already flushed): {:?}",
                String::from_utf8_lossy(&data)
            ),
        }
    });
}

/// Test simulate_crash with close_files=true
#[test]
fn test_simulate_crash_close_files_true() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Create file
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let file = provider
                .open("close_test.txt", OpenOptions::create_write())
                .await?;
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

        // Crash with close_files=true
        sim.simulate_crash(true);

        // File should still be accessible for reopening
        let provider2 = sim.storage_provider();
        let handle2 = tokio::task::spawn_local(async move {
            let exists = provider2.exists("close_test.txt").await?;
            Ok::<_, std::io::Error>(exists)
        });

        while !handle2.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let exists = handle2.await.expect("task panicked").expect("io error");
        println!("File exists after crash (close_files=true): {}", exists);
    });
}

/// Test simulate_crash with close_files=false
#[test]
fn test_simulate_crash_close_files_false() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Create file
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let file = provider
                .open("no_close_test.txt", OpenOptions::create_write())
                .await?;
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

        // Crash with close_files=false
        sim.simulate_crash(false);

        // File should still be accessible
        let provider2 = sim.storage_provider();
        let handle2 = tokio::task::spawn_local(async move {
            let exists = provider2.exists("no_close_test.txt").await?;
            Ok::<_, std::io::Error>(exists)
        });

        while !handle2.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let exists = handle2.await.expect("task panicked").expect("io error");
        println!("File exists after crash (close_files=false): {}", exists);
    });
}

/// Test crash with multiple files
#[test]
fn test_simulate_crash_multiple_files() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Create multiple files
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            for i in 0..5 {
                let mut file = provider
                    .open(&format!("multi_{}.txt", i), OpenOptions::create_write())
                    .await?;
                file.write_all(format!("File {} content", i).as_bytes())
                    .await?;
                file.sync_all().await?;
                drop(file);
            }
            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");

        // Crash affects all files
        sim.simulate_crash(true);

        // All files should still exist (synced data survives)
        let provider2 = sim.storage_provider();
        let handle2 = tokio::task::spawn_local(async move {
            let mut count = 0;
            for i in 0..5 {
                if provider2.exists(&format!("multi_{}.txt", i)).await? {
                    count += 1;
                }
            }
            Ok::<_, std::io::Error>(count)
        });

        while !handle2.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let count = handle2.await.expect("task panicked").expect("io error");
        assert_eq!(count, 5, "All synced files should survive crash");
    });
}

/// Test crash during write operation
#[test]
fn test_simulate_crash_during_write() {
    local_runtime().block_on(async {
        let mut config = StorageConfiguration::fast_local();
        config.crash_fault_probability = 1.0;

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        // Start a write operation
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("mid_write.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"First part").await?;
            // Don't sync - this is "in progress"
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

        // Simulate crash "mid-write" (after write but before sync)
        sim.simulate_crash(true);

        // Verify behavior - data may be lost or corrupted
        let provider2 = sim.storage_provider();
        let handle2 = tokio::task::spawn_local(async move {
            let exists = provider2.exists("mid_write.txt").await?;
            Ok::<_, std::io::Error>(exists)
        });

        while !handle2.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let exists = handle2.await.expect("task panicked").expect("io error");
        println!("File exists after mid-write crash: {}", exists);
    });
}

/// Test that crash_fault_probability=0.0 means no corruption
#[test]
fn test_simulate_crash_zero_corruption_probability() {
    local_runtime().block_on(async {
        let mut config = StorageConfiguration::fast_local();
        config.crash_fault_probability = 0.0; // No corruption

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let data = b"Data with zero crash probability";

        // Write and sync
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("zero_crash.txt", OpenOptions::create_write())
                .await?;
            file.write_all(data).await?;
            file.sync_all().await?;
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

        // Crash with 0% corruption
        sim.simulate_crash(true);

        // Data should be perfectly intact
        let provider2 = sim.storage_provider();
        let handle2 = tokio::task::spawn_local(async move {
            let mut file = provider2
                .open("zero_crash.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            Ok::<_, std::io::Error>(buf)
        });

        while !handle2.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let read_data = handle2.await.expect("task panicked").expect("io error");
        assert_eq!(
            &read_data, data,
            "Data should be intact with 0% crash probability"
        );
    });
}

/// Test repeated crashes on the same simulation
#[test]
fn test_simulate_crash_repeated() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Create file
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("repeated.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"test data").await?;
            file.sync_all().await?;
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

        // Multiple crashes should not panic or cause issues
        for i in 0..10 {
            sim.simulate_crash(i % 2 == 0); // Alternate close_files
        }

        // File should still be accessible
        let provider2 = sim.storage_provider();
        let handle2 =
            tokio::task::spawn_local(async move { provider2.exists("repeated.txt").await });

        while !handle2.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let exists = handle2.await.expect("task panicked").expect("io error");
        println!("File exists after repeated crashes: {}", exists);
    });
}

/// Test crash on empty simulation (no files)
#[test]
fn test_simulate_crash_no_files() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        // Crash with no files - should not panic
        sim.simulate_crash(true);
        sim.simulate_crash(false);

        // Can still create files after crash
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let file = provider
                .open("after_crash.txt", OpenOptions::create_write())
                .await?;
            drop(file);
            provider.exists("after_crash.txt").await
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let exists = handle.await.expect("task panicked").expect("io error");
        assert!(exists, "Should be able to create files after crash");
    });
}
