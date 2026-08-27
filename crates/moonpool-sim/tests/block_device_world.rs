//! `SimWorld` wiring tests for the simulated block device: per-process
//! stores, process-crash and wipe integration, and replay stability.

use std::future::Future;
use std::net::IpAddr;

use moonpool_core::block::SECTOR_SIZE;
use moonpool_core::{BlockDevice, BlockDeviceProvider, BlockError, RegionId, RegionSpec};
use moonpool_sim::{SimWorld, executor::Executor};

const SECTOR: u64 = SECTOR_SIZE as u64;

fn ip(last: u8) -> IpAddr {
    format!("10.0.1.{last}").parse().expect("valid IP")
}

fn run<F: Future>(seed: u64, future: F) -> F::Output {
    Executor::new(seed).block_on(future)
}

fn spec() -> [RegionSpec; 1] {
    [RegionSpec {
        name: "data",
        size: 8 * SECTOR,
    }]
}

/// A process crash resolves that process's block devices through the crash
/// model and records a `block_device_crash` fault, while another process's
/// devices are untouched.
#[test]
fn process_crash_is_scoped_to_the_owning_process() {
    let sim = SimWorld::new_with_seed(42);
    let crashed = sim.block_device_provider(ip(1));
    let bystander = sim.block_device_provider(ip(2));

    let (bystander_device, crashed_device) = run(42, async move {
        let crashed_device = crashed.create("db", &spec()).await.expect("create");
        crashed_device.persist().await.expect("persist");
        crashed_device
            .write(RegionId(0), 0, &vec![0x11; SECTOR_SIZE])
            .await
            .expect("write");

        let bystander_device = bystander.create("db", &spec()).await.expect("create");
        bystander_device.persist().await.expect("persist");
        bystander_device
            .write(RegionId(0), 0, &vec![0x22; SECTOR_SIZE])
            .await
            .expect("write");
        (bystander_device, crashed_device)
    });

    sim.simulate_crash_for_process(ip(1), true);
    let kinds: Vec<&str> = sim
        .take_faults()
        .iter()
        .map(|record| record.event.kind())
        .collect::<Vec<_>>()
        .into_iter()
        .collect();
    assert!(
        kinds.contains(&"block_device_crash"),
        "expected a block_device_crash fault, got {kinds:?}"
    );

    run(42, async move {
        // The bystander's unsynced write is still visible — its store was
        // never crashed.
        let mut buf = vec![0u8; SECTOR_SIZE];
        bystander_device
            .read(RegionId(0), 0, &mut buf)
            .await
            .expect("read bystander");
        assert_eq!(buf, vec![0x22; SECTOR_SIZE]);
        // The crashed process's device still exists (it was persisted once);
        // reads work and reflect the crash-resolved durable image.
        crashed_device
            .read(RegionId(0), 0, &mut buf)
            .await
            .expect("read crashed");
    });
}

/// `wipe_storage_for_process` erases the process's block devices, as a
/// `CrashAndWipe` reboot does.
#[test]
fn wipe_erases_block_devices() {
    let sim = SimWorld::new_with_seed(7);
    let provider = sim.block_device_provider(ip(1));
    let reopen = provider.clone();
    run(7, async move {
        let device = provider.create("db", &spec()).await.expect("create");
        device.persist().await.expect("persist");
    });

    sim.wipe_storage_for_process(ip(1));
    let kinds: Vec<String> = sim
        .take_faults()
        .iter()
        .map(|record| record.event.kind().to_string())
        .collect();
    assert!(
        kinds.iter().any(|kind| kind == "block_device_wipe"),
        "expected a block_device_wipe fault, got {kinds:?}"
    );

    run(7, async move {
        let err = reopen.open("db").await.expect_err("device must be gone");
        assert!(matches!(err, BlockError::NotFound { .. }), "got {err:?}");
    });
}

/// A workload reaches its per-process block device through
/// `SimContext::block_devices()` under the full orchestrator.
#[test]
fn workload_uses_block_devices_through_sim_context() {
    use async_trait::async_trait;
    use moonpool_sim::{SimContext, SimulationBuilder, SimulationResult, Workload};

    struct BlockDeviceWorkload;

    #[async_trait]
    impl Workload for BlockDeviceWorkload {
        fn name(&self) -> &'static str {
            "block_device_workload"
        }

        async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
            let provider = ctx.block_devices();
            let device = provider
                .create("db", &spec())
                .await
                .expect("create block device");
            device
                .write(RegionId(0), 0, &vec![0x5A; SECTOR_SIZE])
                .await
                .expect("write");
            device.persist().await.expect("persist");
            let mut buf = vec![0u8; SECTOR_SIZE];
            device.read(RegionId(0), 0, &mut buf).await.expect("read");
            assert_eq!(buf, vec![0x5A; SECTOR_SIZE]);
            Ok(())
        }
    }

    let report = SimulationBuilder::new()
        .workload(BlockDeviceWorkload)
        .set_iterations(3)
        .run();
    assert_eq!(
        report.failed_runs, 0,
        "block device workload must succeed: {report:?}"
    );
}

/// Per-process store seeds are a pure function of the iteration seed and the
/// IP: replaying the same seed yields bit-identical post-crash contents.
#[test]
fn world_scoped_stores_replay_deterministically() {
    let scenario = |seed: u64| {
        let sim = SimWorld::new_with_seed(seed);
        let provider = sim.block_device_provider(ip(1));
        let device = run(seed, async move {
            let device = provider.create("db", &spec()).await.expect("create");
            device
                .write(RegionId(0), 0, &vec![0xAA; 4 * SECTOR_SIZE])
                .await
                .expect("baseline");
            device.persist().await.expect("persist");
            device
                .write(RegionId(0), 0, &vec![0xBB; 4 * SECTOR_SIZE])
                .await
                .expect("overwrite");
            device
        });
        sim.simulate_crash_for_process(ip(1), true);
        run(seed, async move {
            let mut image = vec![0u8; 8 * SECTOR_SIZE];
            device
                .read(RegionId(0), 0, &mut image)
                .await
                .expect("read image");
            image
        })
    };
    for seed in [1, 55, 1234] {
        assert_eq!(scenario(seed), scenario(seed), "seed {seed} diverged");
    }
}
