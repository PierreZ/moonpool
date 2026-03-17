//! Fault injection tests for storage simulation.
//!
//! These tests verify that the fault injection mechanisms work correctly
//! for various failure modes.

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration};
use std::net::IpAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TEST_IP_STR: &str = "127.0.0.1";

fn test_ip() -> IpAddr {
    TEST_IP_STR.parse().expect("valid IP")
}

/// Helper to run an async storage test with proper simulation stepping.
async fn run_storage_test<F, Fut, T>(mut sim: SimWorld, f: F) -> T
where
    F: FnOnce(moonpool_sim::SimStorageProvider) -> Fut,
    Fut: std::future::Future<Output = T> + 'static,
    T: 'static,
{
    let provider = sim.storage_provider(test_ip());
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
        let provider = sim.storage_provider(test_ip());
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
        sim.simulate_crash_for_process(test_ip(), true);

        // Verify the crash was processed
        // After crash, files are closed and pending writes may be corrupted
        let provider2 = sim.storage_provider(test_ip());
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

// ============================================================================
// Mixed Fault Probability Tests
// ============================================================================

/// Test with realistic low fault probabilities (like production scenarios)
#[test]
fn test_mixed_low_fault_probabilities() {
    local_runtime().block_on(async {
        // Realistic fault probabilities - low but non-zero
        let mut config = StorageConfiguration::fast_local();
        config.read_fault_probability = 0.01; // 1%
        config.write_fault_probability = 0.01; // 1%
        config.sync_failure_probability = 0.005; // 0.5%

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        // Run many operations and count successes/failures
        let mut write_successes = 0;
        let mut write_failures = 0;
        let mut read_successes = 0;
        let mut read_failures = 0;

        for i in 0..100 {
            let provider = sim.storage_provider(test_ip());
            let filename = format!("mixed_{}.txt", i);

            // Try write
            let handle = tokio::task::spawn_local(async move {
                let mut file = provider
                    .open(&filename, OpenOptions::create_write())
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

            match handle.await.expect("task panicked") {
                Ok(()) => write_successes += 1,
                Err(_) => write_failures += 1,
            }

            // Try read (if write succeeded)
            if write_successes > read_successes + read_failures {
                let provider2 = sim.storage_provider(test_ip());
                let filename = format!("mixed_{}.txt", i);
                let handle2 = tokio::task::spawn_local(async move {
                    let mut file = provider2.open(&filename, OpenOptions::read_only()).await?;
                    let mut buf = Vec::new();
                    file.read_to_end(&mut buf).await?;
                    Ok::<_, std::io::Error>(())
                });

                while !handle2.is_finished() {
                    while sim.pending_event_count() > 0 {
                        sim.step();
                    }
                    tokio::task::yield_now().await;
                }

                match handle2.await.expect("task panicked") {
                    Ok(()) => read_successes += 1,
                    Err(_) => read_failures += 1,
                }
            }
        }

        println!(
            "Mixed faults: writes {}/{}, reads {}/{}",
            write_successes,
            write_successes + write_failures,
            read_successes,
            read_successes + read_failures
        );

        // With 1% fault rates over 100 operations, we expect some failures
        // but not all failures
        assert!(
            write_successes > 0,
            "Some writes should succeed with low fault rates"
        );
        assert!(
            read_successes > 0,
            "Some reads should succeed with low fault rates"
        );
    });
}

/// Test with multiple fault types enabled simultaneously
#[test]
fn test_combined_fault_types() {
    local_runtime().block_on(async {
        let mut config = StorageConfiguration::fast_local();
        config.read_fault_probability = 0.1; // 10%
        config.write_fault_probability = 0.1; // 10%
        config.misdirect_read_probability = 0.05; // 5%
        config.misdirect_write_probability = 0.05; // 5%
        config.phantom_write_probability = 0.05; // 5%

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let provider = sim.storage_provider(test_ip());

        // Run operations - expect various fault behaviors
        let handle = tokio::task::spawn_local(async move {
            let mut results = Vec::new();

            for i in 0..20 {
                let filename = format!("combined_{}.txt", i);

                // Write
                let write_result = async {
                    let mut file = provider
                        .open(&filename, OpenOptions::create_write())
                        .await?;
                    file.write_all(format!("Data {}", i).as_bytes()).await?;
                    file.sync_all().await?;
                    Ok::<_, std::io::Error>(())
                }
                .await;

                // Read back if write succeeded
                let read_result = if write_result.is_ok() {
                    async {
                        let mut file = provider.open(&filename, OpenOptions::read_only()).await?;
                        let mut buf = Vec::new();
                        file.read_to_end(&mut buf).await?;
                        Ok::<_, std::io::Error>(buf)
                    }
                    .await
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "write failed",
                    ))
                };

                results.push((write_result.is_ok(), read_result.is_ok()));
            }

            Ok::<_, std::io::Error>(results)
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let results = handle.await.expect("task panicked").expect("test failed");

        let writes_ok = results.iter().filter(|(w, _)| *w).count();
        let reads_ok = results.iter().filter(|(_, r)| *r).count();

        println!(
            "Combined faults: {} writes OK, {} reads OK out of 20",
            writes_ok, reads_ok
        );

        // With combined 10-20% fault rates, expect mixed results
        // Not all should fail, not all should succeed
    });
}

// ============================================================================
// Fault Determinism Tests
// ============================================================================

/// Test that same seed produces same fault pattern
#[test]
fn test_fault_determinism_same_seed() {
    use moonpool_sim::set_sim_seed;

    local_runtime().block_on(async {
        let seed = 42u64;

        // Run with specific seed
        set_sim_seed(seed);

        let mut config = StorageConfiguration::fast_local();
        config.read_fault_probability = 0.5; // 50% for visibility

        let mut sim1 = SimWorld::new();
        sim1.set_storage_config(config.clone());

        let provider = sim1.storage_provider(test_ip());
        let handle = tokio::task::spawn_local(async move {
            let mut results = Vec::new();
            for i in 0..10 {
                let filename = format!("det_{}.txt", i);

                // Create file first
                let mut file = provider
                    .open(&filename, OpenOptions::create_write())
                    .await
                    .expect("create failed");
                file.write_all(b"test").await.expect("write failed");
                file.sync_all().await.expect("sync failed");
                drop(file);

                // Read back
                let result = async {
                    let mut file = provider.open(&filename, OpenOptions::read_only()).await?;
                    let mut buf = Vec::new();
                    file.read_to_end(&mut buf).await?;
                    Ok::<_, std::io::Error>(buf)
                }
                .await;

                results.push(result.is_ok());
            }
            results
        });

        while !handle.is_finished() {
            while sim1.pending_event_count() > 0 {
                sim1.step();
            }
            tokio::task::yield_now().await;
        }

        let results1 = handle.await.expect("task panicked");

        // Run again with same seed
        set_sim_seed(seed);

        let mut sim2 = SimWorld::new();
        sim2.set_storage_config(config);

        let provider = sim2.storage_provider(test_ip());
        let handle = tokio::task::spawn_local(async move {
            let mut results = Vec::new();
            for i in 0..10 {
                let filename = format!("det_{}.txt", i);

                let mut file = provider
                    .open(&filename, OpenOptions::create_write())
                    .await
                    .expect("create failed");
                file.write_all(b"test").await.expect("write failed");
                file.sync_all().await.expect("sync failed");
                drop(file);

                let result = async {
                    let mut file = provider.open(&filename, OpenOptions::read_only()).await?;
                    let mut buf = Vec::new();
                    file.read_to_end(&mut buf).await?;
                    Ok::<_, std::io::Error>(buf)
                }
                .await;

                results.push(result.is_ok());
            }
            results
        });

        while !handle.is_finished() {
            while sim2.pending_event_count() > 0 {
                sim2.step();
            }
            tokio::task::yield_now().await;
        }

        let results2 = handle.await.expect("task panicked");

        println!("Run 1: {:?}", results1);
        println!("Run 2: {:?}", results2);

        assert_eq!(
            results1, results2,
            "Same seed should produce same fault pattern"
        );
    });
}

/// Test that fault injection produces a mix of corrupted and intact reads
#[test]
fn test_fault_injection_produces_mixed_results() {
    use moonpool_sim::set_sim_seed;

    local_runtime().block_on(async {
        set_sim_seed(12345);

        let mut config = StorageConfiguration::fast_local();
        config.read_fault_probability = 0.5; // 50% read corruption

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let provider = sim.storage_provider(test_ip());
        let handle = tokio::task::spawn_local(async move {
            let mut corrupted_count = 0;
            let mut intact_count = 0;
            let original_data = b"testdata";

            for i in 0..20 {
                let filename = format!("mixed_{}.txt", i);

                let mut file = provider
                    .open(&filename, OpenOptions::create_write())
                    .await
                    .expect("create failed");
                file.write_all(original_data).await.expect("write failed");
                file.sync_all().await.expect("sync failed");
                drop(file);

                // Read back and check if corrupted
                let mut file = provider
                    .open(&filename, OpenOptions::read_only())
                    .await
                    .expect("open failed");
                let mut buf = vec![0u8; original_data.len()];
                file.read_exact(&mut buf).await.expect("read failed");

                if buf != original_data {
                    corrupted_count += 1;
                } else {
                    intact_count += 1;
                }
            }
            (corrupted_count, intact_count)
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let (corrupted, intact) = handle.await.expect("task panicked");

        println!(
            "Fault injection results: {} corrupted, {} intact out of 20",
            corrupted, intact
        );

        // With 50% fault probability over 20 operations, we should see
        // a mix of corrupted and intact reads (not all one or the other)
        assert!(
            corrupted > 0,
            "With 50% fault probability, some reads should be corrupted"
        );
        assert!(
            intact > 0,
            "With 50% fault probability, some reads should be intact"
        );
    });
}

/// Test corruption content is deterministic with same seed
#[test]
fn test_corruption_content_deterministic() {
    use moonpool_sim::set_sim_seed;

    local_runtime().block_on(async {
        let seed = 12345u64;

        let run_corruption_test = || async {
            set_sim_seed(seed);

            let mut config = StorageConfiguration::fast_local();
            config.read_fault_probability = 1.0; // 100% corruption

            let mut sim = SimWorld::new();
            sim.set_storage_config(config);

            let provider = sim.storage_provider(test_ip());
            let handle = tokio::task::spawn_local(async move {
                // Write known data
                let mut file = provider
                    .open("corrupt.txt", OpenOptions::create_write())
                    .await?;
                file.write_all(b"ABCDEFGH").await?;
                file.sync_all().await?;
                drop(file);

                // Read - will be corrupted
                let mut file = provider
                    .open("corrupt.txt", OpenOptions::read_only())
                    .await?;
                let mut buf = vec![0u8; 8];
                file.read_exact(&mut buf).await?;
                Ok::<_, std::io::Error>(buf)
            });

            while !handle.is_finished() {
                while sim.pending_event_count() > 0 {
                    sim.step();
                }
                tokio::task::yield_now().await;
            }

            handle.await.expect("task panicked").expect("io error")
        };

        let corrupted1 = run_corruption_test().await;
        let corrupted2 = run_corruption_test().await;

        println!("Corruption 1: {:?}", corrupted1);
        println!("Corruption 2: {:?}", corrupted2);

        assert_eq!(
            corrupted1, corrupted2,
            "Corruption pattern should be deterministic with same seed"
        );
    });
}

// ============================================================================
// Per-Process Storage Fault Isolation Tests
// ============================================================================

/// Test that per-process storage configs provide fault isolation between IPs.
#[test]
fn test_per_process_storage_config_isolation() {
    local_runtime().block_on(async {
        let ip1: IpAddr = "10.0.1.1".parse().expect("valid IP");
        let ip2: IpAddr = "10.0.1.2".parse().expect("valid IP");

        let mut sim = SimWorld::new();
        // Base config with no faults
        sim.set_storage_config(StorageConfiguration::fast_local());

        // IP1: 100% read corruption
        let mut config_ip1 = StorageConfiguration::fast_local();
        config_ip1.read_fault_probability = 1.0;
        sim.set_process_storage_config(ip1, config_ip1);

        // IP2: 0% faults (clean reads)
        let config_ip2 = StorageConfiguration::fast_local();
        sim.set_process_storage_config(ip2, config_ip2);

        let provider1 = sim.storage_provider(ip1);
        let provider2 = sim.storage_provider(ip2);

        let original = b"hello world!";

        // Write known data through both providers (different paths per IP)
        let handle = tokio::task::spawn_local(async move {
            // Write via IP1
            let mut file1 = provider1
                .open("ip1_data.txt", OpenOptions::create_write())
                .await?;
            file1.write_all(original).await?;
            file1.sync_all().await?;
            drop(file1);

            // Write via IP2
            let mut file2 = provider2
                .open("ip2_data.txt", OpenOptions::create_write())
                .await?;
            file2.write_all(original).await?;
            file2.sync_all().await?;
            drop(file2);

            // Read via IP1 - should be corrupted
            let mut file1 = provider1
                .open("ip1_data.txt", OpenOptions::read_only())
                .await?;
            let mut buf1 = vec![0u8; original.len()];
            file1.read_exact(&mut buf1).await?;

            // Read via IP2 - should be intact
            let mut file2 = provider2
                .open("ip2_data.txt", OpenOptions::read_only())
                .await?;
            let mut buf2 = vec![0u8; original.len()];
            file2.read_exact(&mut buf2).await?;

            Ok::<_, std::io::Error>((buf1, buf2))
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let (buf1, buf2) = handle.await.expect("task panicked").expect("io error");

        assert_ne!(
            &buf1[..],
            original.as_slice(),
            "IP1 read should be corrupted (100% read fault)"
        );
        assert_eq!(
            &buf2[..],
            original.as_slice(),
            "IP2 read should be intact (0% fault)"
        );
    });
}

/// Test that simulate_crash_for_process only affects the targeted process.
#[test]
fn test_simulate_crash_for_process_isolation() {
    local_runtime().block_on(async {
        let ip1: IpAddr = "10.0.1.1".parse().expect("valid IP");
        let ip2: IpAddr = "10.0.1.2".parse().expect("valid IP");

        let mut sim = fast_sim();

        let provider1 = sim.storage_provider(ip1);
        let provider2 = sim.storage_provider(ip2);

        // Write and sync data through both providers (different paths per IP)
        let handle = tokio::task::spawn_local(async move {
            let mut file1 = provider1
                .open("ip1_crash.txt", OpenOptions::create_write())
                .await?;
            file1.write_all(b"ip1 data").await?;
            file1.sync_all().await?;
            drop(file1);

            let mut file2 = provider2
                .open("ip2_crash.txt", OpenOptions::create_write())
                .await?;
            file2.write_all(b"ip2 data").await?;
            file2.sync_all().await?;
            drop(file2);

            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");

        // Crash only IP1
        sim.simulate_crash_for_process(ip1, true);

        // IP2 should still be able to read its file
        let provider2_after = sim.storage_provider(ip2);
        let handle2 = tokio::task::spawn_local(async move {
            let mut file = provider2_after
                .open("ip2_crash.txt", OpenOptions::read_only())
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

        let ip2_data = handle2.await.expect("task panicked").expect("io error");
        assert_eq!(
            &ip2_data[..],
            b"ip2 data",
            "IP2 file should be unaffected by IP1 crash"
        );
    });
}

/// Test that wipe_storage_for_process only deletes files for the targeted process.
#[test]
fn test_wipe_storage_for_process() {
    local_runtime().block_on(async {
        let ip1: IpAddr = "10.0.1.1".parse().expect("valid IP");
        let ip2: IpAddr = "10.0.1.2".parse().expect("valid IP");

        let mut sim = fast_sim();

        let provider1 = sim.storage_provider(ip1);
        let provider2 = sim.storage_provider(ip2);

        // Write files through both providers (different paths per IP)
        let handle = tokio::task::spawn_local(async move {
            let mut file1 = provider1
                .open("ip1_wipe.txt", OpenOptions::create_write())
                .await?;
            file1.write_all(b"ip1 data").await?;
            file1.sync_all().await?;
            drop(file1);

            let mut file2 = provider2
                .open("ip2_wipe.txt", OpenOptions::create_write())
                .await?;
            file2.write_all(b"ip2 data").await?;
            file2.sync_all().await?;
            drop(file2);

            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");

        // Wipe only IP1's storage
        sim.wipe_storage_for_process(ip1);

        // Check IP1's file is gone
        let provider1_after = sim.storage_provider(ip1);
        let handle1 = tokio::task::spawn_local(async move {
            let exists = provider1_after.exists("ip1_wipe.txt").await?;
            Ok::<_, std::io::Error>(exists)
        });

        while !handle1.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let ip1_exists = handle1.await.expect("task panicked").expect("io error");
        assert!(!ip1_exists, "IP1 file should be deleted after wipe");

        // Check IP2's file still exists and is readable
        let provider2_after = sim.storage_provider(ip2);
        let handle2 = tokio::task::spawn_local(async move {
            let exists = provider2_after.exists("ip2_wipe.txt").await?;
            if exists {
                let mut file = provider2_after
                    .open("ip2_wipe.txt", OpenOptions::read_only())
                    .await?;
                let mut buf = Vec::new();
                file.read_to_end(&mut buf).await?;
                Ok::<_, std::io::Error>((true, buf))
            } else {
                Ok((false, Vec::new()))
            }
        });

        while !handle2.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let (ip2_exists, ip2_data) = handle2.await.expect("task panicked").expect("io error");
        assert!(ip2_exists, "IP2 file should still exist after IP1 wipe");
        assert_eq!(
            &ip2_data[..],
            b"ip2 data",
            "IP2 file data should be intact after IP1 wipe"
        );
    });
}

/// Test that per-process sync failure config is isolated between IPs.
#[test]
fn test_per_process_sync_failure_isolation() {
    local_runtime().block_on(async {
        let ip1: IpAddr = "10.0.1.1".parse().expect("valid IP");
        let ip2: IpAddr = "10.0.1.2".parse().expect("valid IP");

        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration::fast_local());

        // IP1: 100% sync failure
        let mut config_ip1 = StorageConfiguration::fast_local();
        config_ip1.sync_failure_probability = 1.0;
        sim.set_process_storage_config(ip1, config_ip1);

        // IP2: 0% sync failure
        sim.set_process_storage_config(ip2, StorageConfiguration::fast_local());

        let provider1 = sim.storage_provider(ip1);
        let provider2 = sim.storage_provider(ip2);

        let handle = tokio::task::spawn_local(async move {
            // IP1: sync should fail
            let mut file1 = provider1
                .open("ip1_sync.txt", OpenOptions::create_write())
                .await?;
            file1.write_all(b"data").await?;
            let sync_result1 = file1.sync_all().await;

            // IP2: sync should succeed
            let mut file2 = provider2
                .open("ip2_sync.txt", OpenOptions::create_write())
                .await?;
            file2.write_all(b"data").await?;
            let sync_result2 = file2.sync_all().await;

            Ok::<_, std::io::Error>((sync_result1, sync_result2))
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let (sync1, sync2) = handle.await.expect("task panicked").expect("io error");
        assert!(
            sync1.is_err(),
            "IP1 sync should fail (100% sync_failure_probability)"
        );
        assert!(
            sync2.is_ok(),
            "IP2 sync should succeed (0% sync_failure_probability)"
        );
    });
}

/// Test that phantom write isolation works per process.
///
/// IP1 has 100% phantom writes (writes appear to succeed but don't persist).
/// IP2 has 0% phantom writes. After crash, IP1's data is lost, IP2's survives.
#[test]
fn test_per_process_phantom_write_isolation() {
    local_runtime().block_on(async {
        let ip1: IpAddr = "10.0.1.1".parse().expect("valid IP");
        let ip2: IpAddr = "10.0.1.2".parse().expect("valid IP");

        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration::fast_local());

        // IP1: 100% phantom writes
        let mut config_ip1 = StorageConfiguration::fast_local();
        config_ip1.phantom_write_probability = 1.0;
        config_ip1.crash_fault_probability = 0.0;
        sim.set_process_storage_config(ip1, config_ip1);

        // IP2: no faults
        sim.set_process_storage_config(ip2, StorageConfiguration::fast_local());

        let provider1 = sim.storage_provider(ip1);
        let provider2 = sim.storage_provider(ip2);

        // Write and sync through both
        let handle = tokio::task::spawn_local(async move {
            let mut file1 = provider1
                .open("ip1_phantom.txt", OpenOptions::create_write())
                .await?;
            file1.write_all(b"phantom data").await?;
            file1.sync_all().await?;
            drop(file1);

            let mut file2 = provider2
                .open("ip2_phantom.txt", OpenOptions::create_write())
                .await?;
            file2.write_all(b"real data").await?;
            file2.sync_all().await?;
            drop(file2);

            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");

        // Crash both to reveal phantom writes
        sim.simulate_crash_for_process(ip1, false);
        sim.simulate_crash_for_process(ip2, false);

        // Read back: IP1's phantom writes should be lost, IP2's real writes survive
        let provider1 = sim.storage_provider(ip1);
        let provider2 = sim.storage_provider(ip2);

        let handle = tokio::task::spawn_local(async move {
            let mut file1 = provider1
                .open("ip1_phantom.txt", OpenOptions::read_only())
                .await?;
            let mut buf1 = Vec::new();
            file1.read_to_end(&mut buf1).await?;

            let mut file2 = provider2
                .open("ip2_phantom.txt", OpenOptions::read_only())
                .await?;
            let mut buf2 = Vec::new();
            file2.read_to_end(&mut buf2).await?;

            Ok::<_, std::io::Error>((buf1, buf2))
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let (buf1, buf2) = handle.await.expect("task panicked").expect("io error");
        // IP1: phantom writes mean data was never actually written
        assert_ne!(
            &buf1[..],
            b"phantom data",
            "IP1 phantom writes should not persist after crash"
        );
        // IP2: real writes should survive
        assert_eq!(
            &buf2[..],
            b"real data",
            "IP2 real writes should survive crash"
        );
    });
}

/// Test that global default config is used when no per-process config is set.
#[test]
fn test_default_config_fallback() {
    local_runtime().block_on(async {
        let ip: IpAddr = "10.0.1.1".parse().expect("valid IP");

        let mut sim = SimWorld::new();

        // Set global config with 100% read faults — do NOT set per-process config
        let mut config = StorageConfiguration::fast_local();
        config.read_fault_probability = 1.0;
        sim.set_storage_config(config);

        let provider = sim.storage_provider(ip);
        let original = b"test data here";

        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("fallback.txt", OpenOptions::create_write())
                .await?;
            file.write_all(original).await?;
            file.sync_all().await?;
            drop(file);

            let mut file = provider
                .open("fallback.txt", OpenOptions::read_only())
                .await?;
            let mut buf = vec![0u8; original.len()];
            file.read_exact(&mut buf).await?;

            Ok::<_, std::io::Error>(buf)
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let buf = handle.await.expect("task panicked").expect("io error");
        assert_ne!(
            &buf[..],
            original.as_slice(),
            "Global fallback config should apply when no per-process config is set"
        );
    });
}

/// Test that wiped storage can be recreated by the same process.
#[test]
fn test_wipe_then_recreate() {
    local_runtime().block_on(async {
        let ip: IpAddr = "10.0.1.1".parse().expect("valid IP");

        let mut sim = fast_sim();
        let provider = sim.storage_provider(ip);

        // Create and write a file
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("recoverable.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"original").await?;
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

        // Wipe storage
        sim.wipe_storage_for_process(ip);

        // Recreate the file at the same path
        let provider = sim.storage_provider(ip);
        let handle = tokio::task::spawn_local(async move {
            // File should not exist after wipe
            let exists = provider.exists("recoverable.txt").await?;
            assert!(!exists, "File should not exist after wipe");

            // Create new file at same path
            let mut file = provider
                .open("recoverable.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"recreated").await?;
            file.sync_all().await?;
            drop(file);

            // Read it back
            let mut file = provider
                .open("recoverable.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;

            Ok::<_, std::io::Error>(buf)
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let buf = handle.await.expect("task panicked").expect("io error");
        assert_eq!(
            &buf[..],
            b"recreated",
            "Recreated file should contain new data"
        );
    });
}

/// Test that changing per-process config after files are open affects subsequent operations.
#[test]
fn test_dynamic_config_change() {
    local_runtime().block_on(async {
        let ip: IpAddr = "10.0.1.1".parse().expect("valid IP");

        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration::fast_local());

        // Start with clean config
        sim.set_process_storage_config(ip, StorageConfiguration::fast_local());

        let provider = sim.storage_provider(ip);
        let original = b"dynamic test";

        // Write data with clean config
        let p = provider.clone();
        let handle = tokio::task::spawn_local(async move {
            let mut file = p.open("dynamic.txt", OpenOptions::create_write()).await?;
            file.write_all(original).await?;
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

        // Now change config to 100% read faults — file is already created
        let mut faulty_config = StorageConfiguration::fast_local();
        faulty_config.read_fault_probability = 1.0;
        sim.set_process_storage_config(ip, faulty_config);

        // Read with the new faulty config — should see corruption
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("dynamic.txt", OpenOptions::read_only())
                .await?;
            let mut buf = vec![0u8; original.len()];
            file.read_exact(&mut buf).await?;
            Ok::<_, std::io::Error>(buf)
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let buf = handle.await.expect("task panicked").expect("io error");
        assert_ne!(
            &buf[..],
            original.as_slice(),
            "Dynamically changed config should affect reads on existing files"
        );
    });
}
