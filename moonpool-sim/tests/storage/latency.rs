//! Latency and performance tests for storage simulation.
//!
//! These tests verify that storage operations respect configured latencies
//! and follow the FDB latency formula: base_latency + 1/iops + size/bandwidth

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
fn test_fast_storage_config() {
    local_runtime().block_on(async {
        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration::fast_local());

        let time = run_and_measure_time(sim, |provider| async move {
            let mut file = provider
                .open("fast.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"quick test").await?;
            file.sync_all().await?;
            Ok(())
        })
        .await;

        // Fast config should complete very quickly (microseconds, not milliseconds)
        // Each operation: ~1µs base latency
        // Total: ~3µs (open + write + sync)
        assert!(
            time < Duration::from_millis(1),
            "Fast config should complete in microseconds, got {:?}",
            time
        );
        println!("Fast config completed in {:?}", time);
    });
}

#[test]
fn test_default_storage_config() {
    local_runtime().block_on(async {
        let sim = SimWorld::new(); // Uses default config

        let time = run_and_measure_time(sim, |provider| async move {
            let mut file = provider
                .open("default.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"test").await?;
            file.sync_all().await?;
            Ok(())
        })
        .await;

        // Default config should have realistic delays
        // - Read: 50-200µs
        // - Write: 100-500µs
        // - Sync: 1-5ms
        // So minimum total should be > 1ms due to sync
        assert!(
            time >= Duration::from_millis(1),
            "Default config should take at least 1ms (sync latency), got {:?}",
            time
        );
        println!("Default config completed in {:?}", time);
    });
}

#[test]
fn test_custom_latency_ranges() {
    local_runtime().block_on(async {
        // Create config with known fixed latencies for testing
        let ten_ms = Duration::from_millis(10);
        let config = StorageConfiguration {
            iops: 1_000_000,          // Very high - minimal IOPS overhead
            bandwidth: 1_000_000_000, // Very high - minimal transfer time
            read_latency: ten_ms..ten_ms,
            write_latency: ten_ms..ten_ms,
            sync_latency: ten_ms..ten_ms,
            read_fault_probability: 0.0,
            write_fault_probability: 0.0,
            crash_fault_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,
        };

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let time = run_and_measure_time(sim, |provider| async move {
            let mut file = provider
                .open("custom.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"test").await?;
            file.sync_all().await?;
            Ok(())
        })
        .await;

        // Should be approximately 30ms (write + sync, open uses read latency for metadata)
        // Allow some tolerance
        assert!(
            time >= Duration::from_millis(20),
            "Custom config should respect configured latencies, got {:?}",
            time
        );
        assert!(
            time <= Duration::from_millis(50),
            "Latency should not exceed expected range, got {:?}",
            time
        );
        println!("Custom config completed in {:?}", time);
    });
}

#[test]
fn test_latency_formula() {
    local_runtime().block_on(async {
        // FDB formula: latency = base_latency + 1/iops + size/bandwidth
        // Configure to make IOPS and bandwidth significant
        let base_write = Duration::from_millis(1);
        let config = StorageConfiguration {
            iops: 1000,           // 1ms per operation
            bandwidth: 1_000_000, // 1 MB/s = 1µs per byte
            read_latency: Duration::from_micros(100)..Duration::from_micros(100),
            write_latency: base_write..base_write,
            sync_latency: Duration::from_micros(100)..Duration::from_micros(100),
            read_fault_probability: 0.0,
            write_fault_probability: 0.0,
            crash_fault_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,
        };

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        // Write 10KB of data
        let data_size = 10 * 1024;
        let data = vec![0u8; data_size];

        let time = run_and_measure_time(sim, |provider| async move {
            let mut file = provider
                .open("formula.txt", OpenOptions::create_write())
                .await?;
            file.write_all(&data).await?;
            file.sync_all().await?;
            Ok(())
        })
        .await;

        // Expected write latency for 10KB:
        // base = 1ms
        // iops overhead = 1/1000 = 1ms
        // transfer = 10240 / 1_000_000 = ~10.24ms
        // Total write: ~12.24ms
        // Plus sync: ~0.1ms
        // Plus open (uses read for metadata): ~1.1ms
        // Total: ~13ms minimum

        println!("Latency formula test: wrote {}B in {:?}", data_size, time);
        assert!(
            time >= Duration::from_millis(10),
            "Large write should take noticeable time, got {:?}",
            time
        );
    });
}

#[test]
fn test_read_latency_scales_with_size() {
    local_runtime().block_on(async {
        let base = Duration::from_micros(100);
        let config = StorageConfiguration {
            iops: 1_000_000,
            bandwidth: 100_000, // 100 KB/s - slow to make size effect visible
            read_latency: base..base,
            write_latency: base..base,
            sync_latency: base..base,
            read_fault_probability: 0.0,
            write_fault_probability: 0.0,
            crash_fault_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,
        };

        // First, write test data
        let mut sim1 = SimWorld::new();
        sim1.set_storage_config(config.clone());

        let _ = run_and_measure_time(sim1, |provider| async move {
            let mut file = provider
                .open("read_scale.txt", OpenOptions::create_write())
                .await?;
            file.write_all(&vec![0u8; 10_000]).await?; // 10KB
            file.sync_all().await?;
            Ok(())
        })
        .await;

        // Now measure read times for different sizes
        let sizes = [100, 1000, 5000];
        let mut read_times = Vec::new();

        for &size in &sizes {
            let mut sim = SimWorld::new();
            sim.set_storage_config(config.clone());

            // Pre-create file
            let provider = sim.storage_provider();
            let handle = tokio::task::spawn_local(async move {
                let mut file = provider
                    .open("read_scale.txt", OpenOptions::create_write())
                    .await?;
                file.write_all(&vec![b'x'; 10_000]).await?;
                file.sync_all().await?;
                Ok::<_, std::io::Error>(())
            });

            while !handle.is_finished() {
                while sim.pending_event_count() > 0 {
                    sim.step();
                }
                tokio::task::yield_now().await;
            }
            handle.await.expect("task panicked").expect("io error");

            let start_time = sim.current_time();

            let provider2 = sim.storage_provider();
            let handle2 = tokio::task::spawn_local(async move {
                let mut file = provider2
                    .open("read_scale.txt", OpenOptions::read_only())
                    .await?;
                let mut buf = vec![0u8; size];
                file.read_exact(&mut buf).await?;
                Ok::<_, std::io::Error>(())
            });

            while !handle2.is_finished() {
                while sim.pending_event_count() > 0 {
                    sim.step();
                }
                tokio::task::yield_now().await;
            }
            handle2.await.expect("task panicked").expect("io error");

            let read_time = sim.current_time() - start_time;
            read_times.push((size, read_time));
        }

        println!("Read latency scaling:");
        for (size, time) in &read_times {
            println!("  {} bytes: {:?}", size, time);
        }

        // Larger reads should take longer
        assert!(
            read_times[1].1 > read_times[0].1,
            "1KB read should take longer than 100B read"
        );
        assert!(
            read_times[2].1 > read_times[1].1,
            "5KB read should take longer than 1KB read"
        );
    });
}

#[test]
fn test_write_latency_scales_with_size() {
    local_runtime().block_on(async {
        let base = Duration::from_micros(100);
        let config = StorageConfiguration {
            iops: 1_000_000,
            bandwidth: 100_000, // 100 KB/s - slow to make size effect visible
            read_latency: base..base,
            write_latency: base..base,
            sync_latency: base..base,
            read_fault_probability: 0.0,
            write_fault_probability: 0.0,
            crash_fault_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,
        };

        let sizes = [100, 1000, 5000];
        let mut write_times = Vec::new();

        for &size in &sizes {
            let mut sim = SimWorld::new();
            sim.set_storage_config(config.clone());

            let data = vec![b'x'; size];
            let time = run_and_measure_time(sim, |provider| async move {
                let mut file = provider
                    .open("write_scale.txt", OpenOptions::create_write())
                    .await?;
                file.write_all(&data).await?;
                // No sync - just measure write latency
                Ok(())
            })
            .await;

            write_times.push((size, time));
        }

        println!("Write latency scaling:");
        for (size, time) in &write_times {
            println!("  {} bytes: {:?}", size, time);
        }

        // Larger writes should take longer
        assert!(
            write_times[1].1 > write_times[0].1,
            "1KB write should take longer than 100B write"
        );
        assert!(
            write_times[2].1 > write_times[1].1,
            "5KB write should take longer than 1KB write"
        );
    });
}
