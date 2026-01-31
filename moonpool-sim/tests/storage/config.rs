//! Configuration tests for StorageConfiguration.
//!
//! These tests verify that StorageConfiguration presets and customization
//! work correctly, following the same pattern as network configuration tests.

use moonpool_sim::{SimWorld, StorageConfiguration, set_sim_seed};
use std::time::Duration;

/// Create a local tokio runtime for tests.
fn local_runtime() -> tokio::runtime::LocalRuntime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime")
}

/// Test that fast_local() configuration has expected values
#[test]
fn test_fast_local_configuration_values() {
    let config = StorageConfiguration::fast_local();

    // High IOPS and bandwidth
    assert_eq!(config.iops, 1_000_000);
    assert_eq!(config.bandwidth, 1_000_000_000); // 1 GB/s

    // Minimal latencies (1µs)
    let one_us = Duration::from_micros(1);
    assert_eq!(config.read_latency.start, one_us);
    assert_eq!(config.read_latency.end, one_us);
    assert_eq!(config.write_latency.start, one_us);
    assert_eq!(config.write_latency.end, one_us);
    assert_eq!(config.sync_latency.start, one_us);
    assert_eq!(config.sync_latency.end, one_us);

    // All faults disabled
    assert_eq!(config.read_fault_probability, 0.0);
    assert_eq!(config.write_fault_probability, 0.0);
    assert_eq!(config.crash_fault_probability, 0.0);
    assert_eq!(config.misdirect_write_probability, 0.0);
    assert_eq!(config.misdirect_read_probability, 0.0);
    assert_eq!(config.phantom_write_probability, 0.0);
    assert_eq!(config.sync_failure_probability, 0.0);
}

/// Test that default() configuration has expected SATA SSD values
#[test]
fn test_default_configuration_values() {
    let config = StorageConfiguration::default();

    // SATA SSD values
    assert_eq!(config.iops, 25_000);
    assert_eq!(config.bandwidth, 150_000_000); // 150 MB/s

    // Realistic latency ranges
    assert_eq!(config.read_latency.start, Duration::from_micros(50));
    assert_eq!(config.read_latency.end, Duration::from_micros(200));
    assert_eq!(config.write_latency.start, Duration::from_micros(100));
    assert_eq!(config.write_latency.end, Duration::from_micros(500));
    assert_eq!(config.sync_latency.start, Duration::from_millis(1));
    assert_eq!(config.sync_latency.end, Duration::from_millis(5));

    // All faults disabled by default
    assert_eq!(config.read_fault_probability, 0.0);
    assert_eq!(config.write_fault_probability, 0.0);
    assert_eq!(config.crash_fault_probability, 0.0);
    assert_eq!(config.misdirect_write_probability, 0.0);
    assert_eq!(config.misdirect_read_probability, 0.0);
    assert_eq!(config.phantom_write_probability, 0.0);
    assert_eq!(config.sync_failure_probability, 0.0);
}

/// Test that new() equals default()
#[test]
fn test_new_equals_default() {
    let config_new = StorageConfiguration::new();
    let config_default = StorageConfiguration::default();

    assert_eq!(config_new.iops, config_default.iops);
    assert_eq!(config_new.bandwidth, config_default.bandwidth);
    assert_eq!(config_new.read_latency, config_default.read_latency);
    assert_eq!(config_new.write_latency, config_default.write_latency);
    assert_eq!(config_new.sync_latency, config_default.sync_latency);
}

/// Test that random_for_seed() produces deterministic configs for same seed
#[test]
fn test_random_for_seed_determinism() {
    local_runtime().block_on(async {
        let seed = 12345u64;

        // Create config with first seed
        set_sim_seed(seed);
        let config1 = StorageConfiguration::random_for_seed();

        // Create config with same seed again
        set_sim_seed(seed);
        let config2 = StorageConfiguration::random_for_seed();

        // Should be identical
        assert_eq!(config1.iops, config2.iops, "IOPS should be deterministic");
        assert_eq!(
            config1.bandwidth, config2.bandwidth,
            "Bandwidth should be deterministic"
        );
        assert_eq!(
            config1.read_latency, config2.read_latency,
            "Read latency should be deterministic"
        );
        assert_eq!(
            config1.write_latency, config2.write_latency,
            "Write latency should be deterministic"
        );
        assert_eq!(
            config1.sync_latency, config2.sync_latency,
            "Sync latency should be deterministic"
        );
        assert_eq!(
            config1.read_fault_probability, config2.read_fault_probability,
            "Read fault probability should be deterministic"
        );
        assert_eq!(
            config1.write_fault_probability, config2.write_fault_probability,
            "Write fault probability should be deterministic"
        );
    });
}

/// Test that random_for_seed() produces different configs for different seeds
#[test]
fn test_random_for_seed_varies_by_seed() {
    local_runtime().block_on(async {
        // Create configs with different seeds
        set_sim_seed(111);
        let config1 = StorageConfiguration::random_for_seed();

        set_sim_seed(222);
        let config2 = StorageConfiguration::random_for_seed();

        set_sim_seed(333);
        let config3 = StorageConfiguration::random_for_seed();

        // At least some values should differ between configs
        // (Could theoretically be the same by chance, but very unlikely)
        let all_same_iops = config1.iops == config2.iops && config2.iops == config3.iops;
        let all_same_bandwidth =
            config1.bandwidth == config2.bandwidth && config2.bandwidth == config3.bandwidth;

        // Very unlikely both IOPS and bandwidth are identical across 3 different seeds
        assert!(
            !all_same_iops || !all_same_bandwidth,
            "Different seeds should produce different configs"
        );

        println!("Seed 111: IOPS={}, BW={}", config1.iops, config1.bandwidth);
        println!("Seed 222: IOPS={}, BW={}", config2.iops, config2.bandwidth);
        println!("Seed 333: IOPS={}, BW={}", config3.iops, config3.bandwidth);
    });
}

/// Test that random_for_seed() produces values in expected ranges
#[test]
fn test_random_for_seed_value_ranges() {
    local_runtime().block_on(async {
        // Test multiple seeds to verify range constraints
        for seed in 1..10 {
            set_sim_seed(seed);
            let config = StorageConfiguration::random_for_seed();

            // IOPS should be in 10,000-100,000 range
            assert!(
                config.iops >= 10_000 && config.iops < 100_000,
                "IOPS {} should be in range [10000, 100000)",
                config.iops
            );

            // Bandwidth should be in 50-500 MB/s range
            assert!(
                config.bandwidth >= 50_000_000 && config.bandwidth < 500_000_000,
                "Bandwidth {} should be in range [50M, 500M)",
                config.bandwidth
            );

            // Fault probabilities should be very low (0.001% to 0.1%)
            assert!(
                config.read_fault_probability >= 0.0 && config.read_fault_probability <= 0.001,
                "Read fault prob {} too high",
                config.read_fault_probability
            );
            assert!(
                config.write_fault_probability >= 0.0 && config.write_fault_probability <= 0.001,
                "Write fault prob {} too high",
                config.write_fault_probability
            );
        }
    });
}

/// Test configuration cloning
#[test]
fn test_configuration_clone() {
    let original = StorageConfiguration::default();
    let cloned = original.clone();

    assert_eq!(original.iops, cloned.iops);
    assert_eq!(original.bandwidth, cloned.bandwidth);
    assert_eq!(original.read_latency, cloned.read_latency);
    assert_eq!(original.write_latency, cloned.write_latency);
    assert_eq!(original.sync_latency, cloned.sync_latency);
    assert_eq!(
        original.read_fault_probability,
        cloned.read_fault_probability
    );
}

/// Test custom configuration creation
#[test]
fn test_custom_configuration() {
    let custom = StorageConfiguration {
        iops: 50_000,
        bandwidth: 200_000_000,
        read_latency: Duration::from_micros(30)..Duration::from_micros(100),
        write_latency: Duration::from_micros(50)..Duration::from_micros(200),
        sync_latency: Duration::from_millis(2)..Duration::from_millis(8),
        read_fault_probability: 0.01,
        write_fault_probability: 0.02,
        crash_fault_probability: 0.001,
        misdirect_write_probability: 0.0005,
        misdirect_read_probability: 0.0005,
        phantom_write_probability: 0.001,
        sync_failure_probability: 0.005,
    };

    // Verify all fields are set correctly
    assert_eq!(custom.iops, 50_000);
    assert_eq!(custom.bandwidth, 200_000_000);
    assert_eq!(custom.read_latency.start, Duration::from_micros(30));
    assert_eq!(custom.read_latency.end, Duration::from_micros(100));
    assert_eq!(custom.read_fault_probability, 0.01);
    assert_eq!(custom.write_fault_probability, 0.02);
    assert_eq!(custom.crash_fault_probability, 0.001);
}

/// Test that set_storage_config applies to SimWorld
#[test]
fn test_simworld_set_storage_config() {
    local_runtime().block_on(async {
        let mut sim = SimWorld::new();

        // Default config
        let default_iops = sim.with_storage_config(|c| c.iops);
        assert_eq!(default_iops, 25_000);

        // Apply fast_local
        sim.set_storage_config(StorageConfiguration::fast_local());
        let fast_iops = sim.with_storage_config(|c| c.iops);
        assert_eq!(fast_iops, 1_000_000);

        // Apply custom config
        let custom = StorageConfiguration {
            iops: 42_000,
            ..StorageConfiguration::default()
        };
        sim.set_storage_config(custom);
        let custom_iops = sim.with_storage_config(|c| c.iops);
        assert_eq!(custom_iops, 42_000);
    });
}

/// Test HDD-like configuration
#[test]
fn test_hdd_like_configuration() {
    let hdd_config = StorageConfiguration {
        iops: 150,              // HDDs are very slow on random I/O
        bandwidth: 150_000_000, // Sequential is decent (~150 MB/s)
        read_latency: Duration::from_millis(5)..Duration::from_millis(15),
        write_latency: Duration::from_millis(5)..Duration::from_millis(15),
        sync_latency: Duration::from_millis(10)..Duration::from_millis(50),
        read_fault_probability: 0.0,
        write_fault_probability: 0.0,
        crash_fault_probability: 0.0,
        misdirect_write_probability: 0.0,
        misdirect_read_probability: 0.0,
        phantom_write_probability: 0.0,
        sync_failure_probability: 0.0,
    };

    assert_eq!(hdd_config.iops, 150);
    assert!(hdd_config.read_latency.start >= Duration::from_millis(1));
}

/// Test NVMe-like configuration
#[test]
fn test_nvme_like_configuration() {
    let nvme_config = StorageConfiguration {
        iops: 500_000,            // NVMe can do 500K+ IOPS
        bandwidth: 3_500_000_000, // ~3.5 GB/s
        read_latency: Duration::from_micros(10)..Duration::from_micros(50),
        write_latency: Duration::from_micros(10)..Duration::from_micros(50),
        sync_latency: Duration::from_micros(100)..Duration::from_micros(500),
        read_fault_probability: 0.0,
        write_fault_probability: 0.0,
        crash_fault_probability: 0.0,
        misdirect_write_probability: 0.0,
        misdirect_read_probability: 0.0,
        phantom_write_probability: 0.0,
        sync_failure_probability: 0.0,
    };

    assert_eq!(nvme_config.iops, 500_000);
    assert!(nvme_config.read_latency.start < Duration::from_micros(100));
}

/// Test fault probability boundaries
#[test]
fn test_fault_probability_boundaries() {
    // Test with 0% faults
    let no_faults = StorageConfiguration::fast_local();
    assert_eq!(no_faults.read_fault_probability, 0.0);

    // Test with 100% faults (for testing)
    let all_faults = StorageConfiguration {
        read_fault_probability: 1.0,
        write_fault_probability: 1.0,
        crash_fault_probability: 1.0,
        misdirect_write_probability: 1.0,
        misdirect_read_probability: 1.0,
        phantom_write_probability: 1.0,
        sync_failure_probability: 1.0,
        ..StorageConfiguration::fast_local()
    };
    assert_eq!(all_faults.read_fault_probability, 1.0);
    assert_eq!(all_faults.sync_failure_probability, 1.0);

    // Test with fractional probabilities
    let partial_faults = StorageConfiguration {
        read_fault_probability: 0.001,
        write_fault_probability: 0.0001,
        ..StorageConfiguration::fast_local()
    };
    assert!((partial_faults.read_fault_probability - 0.001).abs() < f64::EPSILON);
}

/// Test Debug implementation
#[test]
fn test_configuration_debug() {
    let config = StorageConfiguration::fast_local();
    let debug_str = format!("{:?}", config);

    // Should contain relevant field names
    assert!(debug_str.contains("iops"));
    assert!(debug_str.contains("bandwidth"));
    assert!(debug_str.contains("read_latency"));
}
