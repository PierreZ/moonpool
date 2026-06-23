//! Latency and performance tests for storage simulation.
//!
//! These tests verify that storage operations respect configured latencies
//! and follow the FDB latency formula: `base_latency` + 1/iops + size/bandwidth

use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{LatencyDistribution, SimWorld, StorageConfiguration};
use std::net::IpAddr;
use std::time::Duration;

/// Build a uniform latency distribution over `[start, end)` for test configs.
fn uniform(start: Duration, end: Duration) -> LatencyDistribution {
    LatencyDistribution::Uniform { start, end }
}

const TEST_IP_STR: &str = "127.0.0.1";

fn test_ip() -> IpAddr {
    TEST_IP_STR.parse().expect("valid IP")
}

/// Run a storage test and return the simulation time elapsed.
async fn run_and_measure_time<F, Fut>(mut sim: SimWorld, f: F) -> Duration
where
    F: FnOnce(moonpool_sim::SimStorageProvider) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<()>> + Send + 'static,
{
    let provider = sim.storage_provider(test_ip());
    let handle = tokio::spawn(f(provider));

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
fn local_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
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
            "Fast config should complete in microseconds, got {time:?}"
        );
        println!("Fast config completed in {time:?}");
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
            "Default config should take at least 1ms (sync latency), got {time:?}"
        );
        println!("Default config completed in {time:?}");
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
            read_latency: uniform(ten_ms, ten_ms),
            write_latency: uniform(ten_ms, ten_ms),
            sync_latency: uniform(ten_ms, ten_ms),
            read_fault_probability: 0.0,
            write_fault_probability: 0.0,
            crash_fault_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,
            ..StorageConfiguration::default()
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
            "Custom config should respect configured latencies, got {time:?}"
        );
        assert!(
            time <= Duration::from_millis(50),
            "Latency should not exceed expected range, got {time:?}"
        );
        println!("Custom config completed in {time:?}");
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
            read_latency: uniform(Duration::from_micros(100), Duration::from_micros(100)),
            write_latency: uniform(base_write, base_write),
            sync_latency: uniform(Duration::from_micros(100), Duration::from_micros(100)),
            read_fault_probability: 0.0,
            write_fault_probability: 0.0,
            crash_fault_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,
            ..StorageConfiguration::default()
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

        println!("Latency formula test: wrote {data_size}B in {time:?}");
        assert!(
            time >= Duration::from_millis(10),
            "Large write should take noticeable time, got {time:?}"
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
            read_latency: uniform(base, base),
            write_latency: uniform(base, base),
            sync_latency: uniform(base, base),
            read_fault_probability: 0.0,
            write_fault_probability: 0.0,
            crash_fault_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,
            ..StorageConfiguration::default()
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
            let provider = sim.storage_provider(test_ip());
            let handle = tokio::spawn(async move {
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

            let provider2 = sim.storage_provider(test_ip());
            let handle2 = tokio::spawn(async move {
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

            let read_time = sim.current_time().checked_sub(start_time).unwrap();
            read_times.push((size, read_time));
        }

        println!("Read latency scaling:");
        for (size, time) in &read_times {
            println!("  {size} bytes: {time:?}");
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
            read_latency: uniform(base, base),
            write_latency: uniform(base, base),
            sync_latency: uniform(base, base),
            read_fault_probability: 0.0,
            write_fault_probability: 0.0,
            crash_fault_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,
            ..StorageConfiguration::default()
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
            println!("  {size} bytes: {time:?}");
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

// =============================================================================
// Dynamic disk stall/throttle episodes (issue #126)
// =============================================================================

/// Workload: open a file and perform `n` write+sync cycles.
async fn write_sync_workload(
    provider: moonpool_sim::SimStorageProvider,
    n: usize,
) -> std::io::Result<()> {
    let mut file = provider
        .open("episodes.txt", OpenOptions::create_write())
        .await?;
    for _ in 0..n {
        file.write_all(b"x").await?;
        file.sync_all().await?;
    }
    Ok(())
}

/// Disk stalls during an episode should freeze I/O and inflate elapsed time,
/// surfacing the backpressure that steady-state timing never produces.
#[test]
fn test_disk_stall_inflates_elapsed_time() {
    local_runtime().block_on(async {
        // Baseline: episodes off (fast_local), the same workload is negligible.
        let mut baseline_sim = SimWorld::new();
        baseline_sim.set_storage_config(StorageConfiguration::fast_local());
        let baseline = run_and_measure_time(baseline_sim, |p| write_sync_workload(p, 5)).await;

        // A stall on every I/O: each op waits out a 50ms freeze window.
        let stall_config = StorageConfiguration {
            disk_stall_probability: 1.0,
            disk_stall_duration: Duration::from_millis(50),
            ..StorageConfiguration::fast_local()
        };
        let mut sim = SimWorld::new();
        sim.set_storage_config(stall_config);
        let stalled = run_and_measure_time(sim, |p| write_sync_workload(p, 5)).await;

        assert!(
            baseline < Duration::from_millis(10),
            "fast_local baseline should be negligible, got {baseline:?}"
        );
        assert!(
            stalled >= Duration::from_millis(200),
            "disk stalls should inflate elapsed time (backpressure), got {stalled:?}"
        );
        assert!(
            stalled > baseline * 50,
            "stalled run ({stalled:?}) should dwarf baseline ({baseline:?})"
        );
    });
}

/// Stall timing is deterministic: the same seed replays the same episodes.
#[test]
fn test_disk_stall_deterministic_per_seed() {
    local_runtime().block_on(async {
        let make_config = || StorageConfiguration {
            disk_stall_probability: 0.5,
            disk_stall_duration: Duration::from_millis(30),
            ..StorageConfiguration::fast_local()
        };

        let mut sim_a = SimWorld::new_with_seed(12_345);
        sim_a.set_storage_config(make_config());
        let a = run_and_measure_time(sim_a, |p| write_sync_workload(p, 8)).await;

        let mut sim_b = SimWorld::new_with_seed(12_345);
        sim_b.set_storage_config(make_config());
        let b = run_and_measure_time(sim_b, |p| write_sync_workload(p, 8)).await;

        assert_eq!(a, b, "same seed must produce identical stall timing");
    });
}

/// With episodes disabled (the default), no stall inflation occurs and the RNG
/// stream is untouched — the workload stays in steady-state timing.
#[test]
fn test_disk_episodes_off_by_default() {
    local_runtime().block_on(async {
        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration::fast_local());
        let elapsed = run_and_measure_time(sim, |p| write_sync_workload(p, 10)).await;

        assert!(
            elapsed < Duration::from_millis(10),
            "disabled episodes should not inflate timing, got {elapsed:?}"
        );
    });
}
