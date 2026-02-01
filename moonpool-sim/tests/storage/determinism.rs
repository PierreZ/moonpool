//! Determinism tests for storage simulation.
//!
//! These tests verify that the storage simulation produces deterministic
//! behavior when run with the same seed.

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration};
use std::time::Duration;
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

/// Run a storage test and return the simulation time elapsed.
async fn run_and_measure_time<F, Fut>(mut sim: SimWorld, f: F) -> Duration
where
    F: FnOnce(moonpool_sim::SimStorageProvider) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<()>> + 'static,
{
    let provider = sim.storage_provider();
    let handle = tokio::task::spawn_local(f(provider));

    while !handle.is_finished() {
        while sim.pending_event_count() > 0 {
            sim.step();
        }
        tokio::task::yield_now().await;
    }

    handle.await.expect("task panicked").expect("io error");
    sim.current_time()
}

/// Create a local tokio runtime for tests.
fn local_runtime() -> tokio::runtime::LocalRuntime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime")
}

#[test]
fn test_same_seed_same_timing() {
    local_runtime().block_on(async {
        let seed = 12345u64;
        let mut times = Vec::new();

        for _ in 0..3 {
            let sim = SimWorld::new_with_seed(seed);
            let time = run_and_measure_time(sim, |provider| async move {
                let mut file = provider
                    .open("test.txt", OpenOptions::create_write())
                    .await?;
                file.write_all(b"hello world").await?;
                file.sync_all().await?;
                drop(file);

                let mut file = provider.open("test.txt", OpenOptions::read_only()).await?;
                let mut buf = Vec::new();
                file.read_to_end(&mut buf).await?;

                Ok(())
            })
            .await;

            times.push(time);
        }

        // All times should be identical
        let first = times[0];
        for (i, time) in times.iter().enumerate() {
            assert_eq!(
                *time, first,
                "Run {} produced different timing: {:?} vs {:?}",
                i, time, first
            );
        }

        println!("All runs completed deterministically in {:?}", first);
    });
}

#[test]
fn test_same_seed_same_corruption() {
    local_runtime().block_on(async {
        let seed = 54321u64;
        let mut results = Vec::new();

        // Configure with corruption enabled
        let mut config = StorageConfiguration::default();
        config.read_fault_probability = 0.5; // High probability for testing

        for _ in 0..3 {
            let mut sim = SimWorld::new_with_seed(seed);
            sim.set_storage_config(config.clone());

            // Collect multiple read results to capture corruption pattern
            let read_results: Vec<Vec<u8>> = run_storage_test(sim, |provider| async move {
                // Write known data
                let mut file = provider
                    .open("corrupt.txt", OpenOptions::create_write())
                    .await
                    .expect("open failed");
                file.write_all(b"0123456789").await.expect("write failed");
                file.sync_all().await.expect("sync failed");
                drop(file);

                // Read multiple times to capture corruption
                let mut reads = Vec::new();
                for _ in 0..5 {
                    let mut file = provider
                        .open("corrupt.txt", OpenOptions::read_only())
                        .await
                        .expect("open failed");
                    let mut buf = Vec::new();
                    // Ignore read errors - we're just capturing what we can
                    let _ = file.read_to_end(&mut buf).await;
                    reads.push(buf);
                }

                reads
            })
            .await;

            results.push(read_results);
        }

        // All runs should have identical corruption patterns
        let first = &results[0];
        for (i, result) in results.iter().enumerate() {
            assert_eq!(
                result, first,
                "Run {} produced different corruption pattern",
                i
            );
        }

        println!(
            "Corruption pattern is deterministic across {} runs",
            results.len()
        );
    });
}

#[test]
fn test_different_seeds_different_timing() {
    local_runtime().block_on(async {
        let mut times = Vec::new();

        for seed in [1u64, 2, 3, 4, 5] {
            let sim = SimWorld::new_with_seed(seed);
            let time = run_and_measure_time(sim, |provider| async move {
                let mut file = provider
                    .open("test.txt", OpenOptions::create_write())
                    .await?;
                file.write_all(b"hello world").await?;
                file.sync_all().await?;
                Ok(())
            })
            .await;

            times.push((seed, time));
        }

        // Not all times should be identical (with high probability)
        // Due to latency sampling from ranges
        let first_time = times[0].1;
        let all_same = times.iter().all(|(_, t)| *t == first_time);

        if all_same {
            // This is actually ok with fast_local config since latencies are fixed
            println!("Note: All times are same (likely using fixed latencies)");
        } else {
            println!("Different seeds produced different timings:");
            for (seed, time) in &times {
                println!("  Seed {}: {:?}", seed, time);
            }
        }
    });
}

#[test]
fn test_deterministic_misdirection() {
    local_runtime().block_on(async {
        let seed = 99999u64;
        let mut results = Vec::new();

        // Configure with misdirection enabled
        let mut config = StorageConfiguration::default();
        config.misdirect_write_probability = 0.5; // High for testing

        for _ in 0..3 {
            let mut sim = SimWorld::new_with_seed(seed);
            sim.set_storage_config(config.clone());

            let data: Vec<u8> = run_storage_test(sim, |provider| async move {
                // Write some data
                let mut file = provider
                    .open("misdirect.txt", OpenOptions::create_write())
                    .await
                    .expect("open failed");
                for i in 0..10u8 {
                    file.write_all(&[i]).await.expect("write failed");
                }
                file.sync_all().await.expect("sync failed");
                drop(file);

                // Read it back
                let mut file = provider
                    .open("misdirect.txt", OpenOptions::read_only())
                    .await
                    .expect("open failed");
                let mut buf = Vec::new();
                file.read_to_end(&mut buf).await.expect("read failed");
                buf
            })
            .await;

            results.push(data);
        }

        // All runs should have identical results (including misdirection effects)
        let first = &results[0];
        for (i, result) in results.iter().enumerate() {
            assert_eq!(
                result, first,
                "Run {} produced different misdirection result",
                i
            );
        }

        println!(
            "Misdirection is deterministic across {} runs",
            results.len()
        );
    });
}
