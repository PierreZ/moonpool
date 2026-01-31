//! IOPS and bandwidth performance tests for storage simulation.
//!
//! These tests verify that the storage simulation correctly applies
//! IOPS limits and bandwidth constraints following the latency formula:
//! `total_latency = base_latency + 1/iops + size/bandwidth`

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Create a local tokio runtime for tests.
fn local_runtime() -> tokio::runtime::LocalRuntime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime")
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

/// Test that low IOPS configuration increases latency for many small operations
#[test]
fn test_low_iops_increases_latency() {
    local_runtime().block_on(async {
        let base = Duration::from_micros(10);

        // High IOPS config
        let high_iops_config = StorageConfiguration {
            iops: 1_000_000, // 1µs per IOPS
            bandwidth: 1_000_000_000,
            read_latency: base..base,
            write_latency: base..base,
            sync_latency: base..base,
            ..StorageConfiguration::fast_local()
        };

        // Low IOPS config
        let low_iops_config = StorageConfiguration {
            iops: 100, // 10ms per IOPS
            bandwidth: 1_000_000_000,
            read_latency: base..base,
            write_latency: base..base,
            sync_latency: base..base,
            ..StorageConfiguration::fast_local()
        };

        // Measure with high IOPS
        let mut sim_high = SimWorld::new();
        sim_high.set_storage_config(high_iops_config);

        let high_time = run_and_measure_time(sim_high, |provider| async move {
            let mut file = provider
                .open("iops_test.txt", OpenOptions::create_write())
                .await?;
            // Many small writes
            for _ in 0..10 {
                file.write_all(b"x").await?;
            }
            Ok(())
        })
        .await;

        // Measure with low IOPS
        let mut sim_low = SimWorld::new();
        sim_low.set_storage_config(low_iops_config);

        let low_time = run_and_measure_time(sim_low, |provider| async move {
            let mut file = provider
                .open("iops_test.txt", OpenOptions::create_write())
                .await?;
            // Same many small writes
            for _ in 0..10 {
                file.write_all(b"x").await?;
            }
            Ok(())
        })
        .await;

        println!("High IOPS (1M): {:?}", high_time);
        println!("Low IOPS (100): {:?}", low_time);

        // Low IOPS should take significantly longer
        assert!(
            low_time > high_time,
            "Low IOPS should take longer: {:?} vs {:?}",
            low_time,
            high_time
        );

        // With 100 IOPS, 10 ops should take at least 100ms (10 * 10ms)
        assert!(
            low_time >= Duration::from_millis(50),
            "Low IOPS operations should take substantial time: {:?}",
            low_time
        );
    });
}

/// Test that low bandwidth increases latency for large transfers
#[test]
fn test_low_bandwidth_increases_latency() {
    local_runtime().block_on(async {
        let base = Duration::from_micros(10);

        // High bandwidth config
        let high_bw_config = StorageConfiguration {
            iops: 1_000_000,
            bandwidth: 1_000_000_000, // 1 GB/s
            read_latency: base..base,
            write_latency: base..base,
            sync_latency: base..base,
            ..StorageConfiguration::fast_local()
        };

        // Low bandwidth config
        let low_bw_config = StorageConfiguration {
            iops: 1_000_000,
            bandwidth: 10_000, // 10 KB/s - very slow
            read_latency: base..base,
            write_latency: base..base,
            sync_latency: base..base,
            ..StorageConfiguration::fast_local()
        };

        let data = vec![0u8; 1024]; // 1KB of data

        // Measure with high bandwidth
        let mut sim_high = SimWorld::new();
        sim_high.set_storage_config(high_bw_config);

        let data_clone = data.clone();
        let high_time = run_and_measure_time(sim_high, |provider| async move {
            let mut file = provider
                .open("bw_test.txt", OpenOptions::create_write())
                .await?;
            file.write_all(&data_clone).await?;
            Ok(())
        })
        .await;

        // Measure with low bandwidth
        let mut sim_low = SimWorld::new();
        sim_low.set_storage_config(low_bw_config);

        let low_time = run_and_measure_time(sim_low, |provider| async move {
            let mut file = provider
                .open("bw_test.txt", OpenOptions::create_write())
                .await?;
            file.write_all(&data).await?;
            Ok(())
        })
        .await;

        println!("High BW (1GB/s): {:?}", high_time);
        println!("Low BW (10KB/s): {:?}", low_time);

        // Low bandwidth should take significantly longer
        assert!(
            low_time > high_time,
            "Low bandwidth should take longer: {:?} vs {:?}",
            low_time,
            high_time
        );

        // 1KB at 10KB/s = 100ms minimum
        assert!(
            low_time >= Duration::from_millis(50),
            "Low bandwidth transfer should take substantial time: {:?}",
            low_time
        );
    });
}

/// Test large file write with bandwidth constraint
#[test]
fn test_large_file_bandwidth_constraint() {
    local_runtime().block_on(async {
        let base = Duration::from_micros(1);
        let config = StorageConfiguration {
            iops: 1_000_000,
            bandwidth: 100_000, // 100 KB/s
            read_latency: base..base,
            write_latency: base..base,
            sync_latency: base..base,
            ..StorageConfiguration::fast_local()
        };

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        // Write 10KB - should take ~100ms at 100KB/s
        let data = vec![0u8; 10 * 1024];

        let time = run_and_measure_time(sim, |provider| async move {
            let mut file = provider
                .open("large.txt", OpenOptions::create_write())
                .await?;
            file.write_all(&data).await?;
            Ok(())
        })
        .await;

        println!("10KB at 100KB/s: {:?}", time);

        // 10KB / 100KB/s = 100ms, allow some margin
        assert!(
            time >= Duration::from_millis(80),
            "Large file should respect bandwidth: {:?}",
            time
        );
        assert!(
            time <= Duration::from_millis(200),
            "Latency shouldn't be excessive: {:?}",
            time
        );
    });
}

/// Test that IOPS and bandwidth effects combine
#[test]
fn test_iops_and_bandwidth_combine() {
    local_runtime().block_on(async {
        let base = Duration::from_micros(1);

        // Both IOPS and bandwidth limited
        let config = StorageConfiguration {
            iops: 100,         // 10ms per op
            bandwidth: 10_000, // 10KB/s
            read_latency: base..base,
            write_latency: base..base,
            sync_latency: base..base,
            ..StorageConfiguration::fast_local()
        };

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        // Write 1KB - IOPS: 10ms, bandwidth: 100ms
        let data = vec![0u8; 1024];

        let time = run_and_measure_time(sim, |provider| async move {
            let mut file = provider
                .open("combined.txt", OpenOptions::create_write())
                .await?;
            file.write_all(&data).await?;
            Ok(())
        })
        .await;

        println!("1KB with IOPS=100, BW=10KB/s: {:?}", time);

        // Should see effects of both constraints
        // At minimum: base + 1/100 + 1024/10000 = 1µs + 10ms + 102ms ≈ 112ms
        assert!(
            time >= Duration::from_millis(50),
            "Combined constraints should add up: {:?}",
            time
        );
    });
}

/// Test read bandwidth constraint
#[test]
fn test_read_bandwidth_constraint() {
    local_runtime().block_on(async {
        let base = Duration::from_micros(1);
        let config = StorageConfiguration {
            iops: 1_000_000,
            bandwidth: 50_000, // 50 KB/s
            read_latency: base..base,
            write_latency: base..base,
            sync_latency: base..base,
            ..StorageConfiguration::fast_local()
        };

        let mut sim = SimWorld::new();
        sim.set_storage_config(config.clone());

        // First create the file
        let provider = sim.storage_provider();
        let handle = tokio::task::spawn_local(async move {
            let mut file = provider
                .open("read_bw.txt", OpenOptions::create_write())
                .await?;
            file.write_all(&vec![b'x'; 5 * 1024]).await?; // 5KB
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

        // Reset time reference
        let start_time = sim.current_time();

        // Now read 5KB - should take ~100ms at 50KB/s
        let provider2 = sim.storage_provider();
        let handle2 = tokio::task::spawn_local(async move {
            let mut file = provider2
                .open("read_bw.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            Ok::<_, std::io::Error>(buf.len())
        });

        while !handle2.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        let bytes_read = handle2.await.expect("task panicked").expect("io error");
        let read_time = sim.current_time() - start_time;

        println!("Read {} bytes at 50KB/s: {:?}", bytes_read, read_time);

        // 5KB / 50KB/s = 100ms
        assert!(
            read_time >= Duration::from_millis(50),
            "Read should respect bandwidth: {:?}",
            read_time
        );
    });
}

/// Test sequential small operations with IOPS limit
#[test]
fn test_sequential_small_ops_iops_limited() {
    local_runtime().block_on(async {
        let base = Duration::from_micros(1);
        let config = StorageConfiguration {
            iops: 50, // 20ms per operation
            bandwidth: 1_000_000_000,
            read_latency: base..base,
            write_latency: base..base,
            sync_latency: base..base,
            ..StorageConfiguration::fast_local()
        };

        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        // 5 write operations
        let time = run_and_measure_time(sim, |provider| async move {
            let mut file = provider
                .open("seq_ops.txt", OpenOptions::create_write())
                .await?;
            for i in 0..5 {
                file.write_all(&[i]).await?;
            }
            Ok(())
        })
        .await;

        println!("5 small ops at 50 IOPS: {:?}", time);

        // 5 ops at 50 IOPS = 5 * 20ms = 100ms minimum
        assert!(
            time >= Duration::from_millis(50),
            "Sequential ops should be IOPS limited: {:?}",
            time
        );
    });
}

/// Test that fast_local config has minimal overhead
#[test]
fn test_fast_local_minimal_overhead() {
    local_runtime().block_on(async {
        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration::fast_local());

        // Large write should still be fast with fast_local
        let data = vec![0u8; 100 * 1024]; // 100KB

        let time = run_and_measure_time(sim, |provider| async move {
            let mut file = provider
                .open("fast.txt", OpenOptions::create_write())
                .await?;
            file.write_all(&data).await?;
            file.sync_all().await?;
            Ok(())
        })
        .await;

        println!("100KB with fast_local: {:?}", time);

        // With 1GB/s bandwidth and 1M IOPS, should complete in microseconds
        assert!(
            time < Duration::from_millis(10),
            "fast_local should have minimal overhead: {:?}",
            time
        );
    });
}

/// Test comparing HDD-like vs SSD-like performance
#[test]
fn test_hdd_vs_ssd_performance() {
    local_runtime().block_on(async {
        // HDD-like: low IOPS, decent sequential bandwidth
        let hdd_config = StorageConfiguration {
            iops: 150,
            bandwidth: 150_000_000, // 150 MB/s sequential
            read_latency: Duration::from_millis(5)..Duration::from_millis(5),
            write_latency: Duration::from_millis(5)..Duration::from_millis(5),
            sync_latency: Duration::from_millis(10)..Duration::from_millis(10),
            ..StorageConfiguration::fast_local()
        };

        // SSD-like: high IOPS
        let ssd_config = StorageConfiguration {
            iops: 50_000,
            bandwidth: 500_000_000, // 500 MB/s
            read_latency: Duration::from_micros(100)..Duration::from_micros(100),
            write_latency: Duration::from_micros(100)..Duration::from_micros(100),
            sync_latency: Duration::from_millis(1)..Duration::from_millis(1),
            ..StorageConfiguration::fast_local()
        };

        // Many small random writes - HDD should be much slower
        let mut sim_hdd = SimWorld::new();
        sim_hdd.set_storage_config(hdd_config);

        let hdd_time = run_and_measure_time(sim_hdd, |provider| async move {
            let mut file = provider
                .open("random.txt", OpenOptions::create_write())
                .await?;
            for i in 0..10u8 {
                file.write_all(&[i]).await?;
            }
            file.sync_all().await?;
            Ok(())
        })
        .await;

        let mut sim_ssd = SimWorld::new();
        sim_ssd.set_storage_config(ssd_config);

        let ssd_time = run_and_measure_time(sim_ssd, |provider| async move {
            let mut file = provider
                .open("random.txt", OpenOptions::create_write())
                .await?;
            for i in 0..10u8 {
                file.write_all(&[i]).await?;
            }
            file.sync_all().await?;
            Ok(())
        })
        .await;

        println!("HDD (150 IOPS): {:?}", hdd_time);
        println!("SSD (50K IOPS): {:?}", ssd_time);

        // HDD should be slower for random I/O
        assert!(
            hdd_time > ssd_time,
            "HDD should be slower for random I/O: {:?} vs {:?}",
            hdd_time,
            ssd_time
        );
    });
}
