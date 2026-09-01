//! Lifecycle tests for the chaos → recovery transition.
//!
//! [`SimWorld::enter_recovery_mode`] is the boundary the runner crosses when
//! `chaos_duration` expires. Its contract has two halves, and both need
//! guarding:
//!
//! 1. **No new faults.** Every configuration-driven fault family — network,
//!    stream storage, block device — stops sampling, and the environmental
//!    partitions the simulator is holding are healed so the system under test
//!    gets a quiet tail it can actually recover in.
//! 2. **Damage already done stays done.** Corrupted sectors stay corrupted,
//!    closed connections stay closed, a degraded link stays slow, and a finite
//!    episode already running expires on its own schedule instead of being
//!    rewritten out of history.
//!
//! Every test here pins probabilities to 0.0 or 1.0 so the interesting path is
//! taken deterministically rather than sampled.

use std::{
    future::Future,
    io::SeekFrom,
    net::IpAddr,
    pin::pin,
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use futures::{
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt},
    task::noop_waker,
};
use moonpool_core::block::SECTOR_SIZE as BLOCK_SECTOR_SIZE;
use moonpool_core::{
    BlockDevice, BlockDeviceProvider, OpenOptions, RegionId, RegionSpec, StorageFile,
    StorageProvider,
};
use moonpool_sim::{
    BlockFaultConfig, Event, FaultContext, FaultInjector, LatencyDistribution,
    NetworkConfiguration, NetworkEvent, NetworkFaultMask, NetworkProvider, PartitionStrategy,
    Process, SECTOR_SIZE, SimContext, SimWorld, SimulationBuilder, SimulationError,
    SimulationResult, StorageConfiguration, TcpListenerTrait, TimeProvider, Workload,
    executor::Executor,
};

/// How many events a driven future may consume before it is declared stuck.
const MAX_DRIVER_STEPS: usize = 10_000;

fn ip(last_octet: u8) -> IpAddr {
    format!("10.0.1.{last_octet}")
        .parse()
        .expect("valid test IP")
}

/// Poll a simulation-backed future, advancing virtual time whenever it parks.
fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);

    for _ in 0..MAX_DRIVER_STEPS {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        assert!(
            sim.has_pending_events(),
            "simulation-backed future stalled without a pending event"
        );
        sim.step();
    }

    panic!("simulation-backed future exceeded {MAX_DRIVER_STEPS} events")
}

fn poll_write_once(
    stream: &mut (impl AsyncWrite + Unpin),
    data: &[u8],
) -> Poll<std::io::Result<usize>> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    std::pin::Pin::new(stream).poll_write(&mut context, data)
}

fn poll_read_once(
    stream: &mut (impl AsyncRead + Unpin),
    data: &mut [u8],
) -> Poll<std::io::Result<usize>> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    std::pin::Pin::new(stream).poll_read(&mut context, data)
}

/// Pump the scheduler by feeding it network maintenance ticks, which is where
/// the engine rolls for a new random partition.
fn tick_maintenance(sim: &mut SimWorld, ticks: usize) {
    for _ in 0..ticks {
        sim.schedule_event(Event::Network(NetworkEvent::Maintenance), Duration::ZERO);
        sim.step();
    }
}

// ===========================================================================
// 1. New configuration-driven faults stop at the boundary
// ===========================================================================

/// Random partitioning is rolled on every event, straight from the network
/// configuration — it never went through the fault-injector token, which is
/// exactly why the cutoff used to leak new cuts into the quiet tail.
#[test]
fn recovery_mode_stops_new_random_partitions() {
    let mut config = NetworkConfiguration::fast_local();
    config.chaos.partition_probability = 1.0;
    config.chaos.partition_duration = Duration::from_millis(50)..Duration::from_millis(51);
    config.chaos.partition_strategy = PartitionStrategy::IsolateSingle;
    let mut sim = SimWorld::new_with_network_config_and_seed(config, 20_260_901);

    // Two connected endpoints give the partition roller something to cut.
    let server = sim.network_provider(ip(2));
    let listener = drive(&mut sim, server.bind("10.0.1.2:8080")).expect("bind");
    let client = sim.network_provider(ip(1));
    let _client_stream = drive(&mut sim, client.connect("10.0.1.2:8080")).expect("connect");
    let (_server_stream, _) = drive(&mut sim, listener.accept()).expect("accept");

    tick_maintenance(&mut sim, 4);
    assert!(
        sim.is_partitioned(ip(1), ip(2)) || sim.is_partitioned(ip(2), ip(1)),
        "the chaos phase must actually partition, or the test proves nothing"
    );

    sim.enter_recovery_mode();
    assert!(sim.is_in_recovery_mode());
    assert!(
        !sim.is_partitioned(ip(1), ip(2)) && !sim.is_partitioned(ip(2), ip(1)),
        "recovery mode heals the partitions in force at the boundary"
    );

    // A partition would be rolled on *every* one of these ticks before.
    tick_maintenance(&mut sim, 200);
    assert!(
        !sim.is_partitioned(ip(1), ip(2)) && !sim.is_partitioned(ip(2), ip(1)),
        "no new partition may be created after the recovery boundary"
    );
}

/// The asymmetric cuts: `send_partitions` and `recv_partitions` are keyed by a
/// single IP, so the old pairwise `restore_partition` sweep could not reach
/// them and a one-way cut outlived the cutoff.
#[test]
fn recovery_mode_heals_asymmetric_partitions() {
    let sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());
    let mut sim = sim;

    sim.partition_pair(ip(1), ip(2), Duration::from_hours(1));
    sim.partition_send_from(ip(3), Duration::from_hours(1));
    sim.partition_recv_to(ip(4), Duration::from_hours(1));
    assert!(sim.is_partitioned(ip(1), ip(2)));
    assert!(
        sim.is_partitioned(ip(3), ip(9)),
        "send-side cut is in force"
    );
    assert!(
        sim.is_partitioned(ip(9), ip(4)),
        "recv-side cut is in force"
    );

    sim.enter_recovery_mode();

    assert!(!sim.is_partitioned(ip(1), ip(2)));
    assert!(!sim.is_partitioned(ip(3), ip(9)), "send-side cut healed");
    assert!(!sim.is_partitioned(ip(9), ip(4)), "recv-side cut healed");

    let kinds: Vec<&str> = sim
        .take_faults()
        .iter()
        .map(|record| record.event.kind())
        .collect();
    assert!(
        kinds.contains(&"partition_healed")
            && kinds.contains(&"send_partition_healed")
            && kinds.contains(&"recv_partition_healed"),
        "every heal is recorded on the fault timeline, got {kinds:?}"
    );
}

/// Healing at the boundary must let the endpoints talk again *now*, not when
/// the (possibly hour-long) partition deadline it stalled under expires.
#[test]
fn endpoints_communicate_again_after_the_recovery_boundary() {
    const PAYLOAD: &[u8] = b"held-back-by-the-cut";

    let mut sim =
        SimWorld::new_with_network_config_and_seed(NetworkConfiguration::fast_local(), 20_260_901);
    let server_provider = sim.network_provider(ip(2));
    let listener = drive(&mut sim, server_provider.bind("10.0.1.2:8080")).expect("bind");
    let client_provider = sim.network_provider(ip(1));
    let mut client = drive(&mut sim, client_provider.connect("10.0.1.2:8080")).expect("connect");
    let (mut server, _) = drive(&mut sim, listener.accept()).expect("accept");
    sim.run_until_empty();

    sim.partition_pair(ip(1), ip(2), Duration::from_hours(1));
    assert_eq!(
        poll_write_once(&mut client, PAYLOAD).map(|result| result.expect("payload is accepted")),
        Poll::Ready(PAYLOAD.len())
    );
    assert!(sim.step(), "the engine drains the queued send");
    let mut byte = [0_u8; 1];
    assert!(
        poll_read_once(&mut server, &mut byte).is_pending(),
        "no byte crosses an active partition"
    );

    sim.enter_recovery_mode();

    let mut received = vec![0_u8; PAYLOAD.len()];
    drive(&mut sim, server.read_exact(&mut received)).expect("the stalled bytes are released");
    assert_eq!(received, PAYLOAD);
    assert!(
        sim.current_time() < Duration::from_hours(1),
        "the bytes were released by the heal, not by the partition deadline"
    );
}

/// Recovery mode stops the environment from breaking connections. It does not
/// mend one the application has already seen close.
#[test]
fn a_connection_already_closed_stays_closed_through_recovery_mode() {
    let mut sim =
        SimWorld::new_with_network_config_and_seed(NetworkConfiguration::fast_local(), 20_260_901);
    let server_provider = sim.network_provider(ip(2));
    let listener = drive(&mut sim, server_provider.bind("10.0.1.2:8080")).expect("bind");
    let client_provider = sim.network_provider(ip(1));
    let client = drive(&mut sim, client_provider.connect("10.0.1.2:8080")).expect("connect");
    let (_server, _) = drive(&mut sim, listener.accept()).expect("accept");
    sim.run_until_empty();

    let connection = client.connection_id();
    sim.close_connection_abort(connection);
    assert!(sim.is_connection_closed(connection));

    sim.enter_recovery_mode();

    assert!(
        sim.is_connection_closed(connection),
        "recovery mode must not resurrect a connection the application already saw close"
    );
}

// ===========================================================================
// 2. Persistent storage damage survives
// ===========================================================================

/// A write fault marks the written sectors as faulted, so every later read of
/// them comes back corrupted. After the boundary: no *new* sector is faulted,
/// and the sector already faulted is still faulted, byte for byte.
#[test]
fn storage_damage_survives_recovery_mode_while_new_faults_stop() {
    let mut config = StorageConfiguration::fast_local();
    config.write_fault_probability = 1.0;
    let mut sim = SimWorld::new_with_seed(20_260_901);
    sim.set_storage_config(config);
    let provider = sim.storage_provider(ip(1));

    let damaged = vec![0xAA_u8; SECTOR_SIZE];
    let clean = vec![0xBB_u8; SECTOR_SIZE];
    let mut file = drive(
        &mut sim,
        provider.open(
            "damaged.bin",
            OpenOptions::new().read(true).write(true).create(true),
        ),
    )
    .expect("open");

    // --- chaos phase: the write lands but its sectors are faulted ---
    let corrupted = drive(&mut sim, async {
        file.write_all(&damaged).await.expect("write sector 0");
        file.sync_all().await.expect("sync");
        file.seek(SeekFrom::Start(0)).await.expect("seek");
        let mut buf = vec![0_u8; SECTOR_SIZE];
        file.read_exact(&mut buf).await.expect("read sector 0");
        buf
    });
    assert_ne!(
        corrupted, damaged,
        "the chaos phase must actually corrupt, or the test proves nothing"
    );

    sim.enter_recovery_mode();

    // --- quiet tail: a fresh sector takes no new damage ---
    let fresh = drive(&mut sim, async {
        file.seek(SeekFrom::Start(SECTOR_SIZE as u64))
            .await
            .expect("seek");
        file.write_all(&clean).await.expect("write sector 1");
        file.sync_all().await.expect("sync");
        file.seek(SeekFrom::Start(SECTOR_SIZE as u64))
            .await
            .expect("seek");
        let mut buf = vec![0_u8; SECTOR_SIZE];
        file.read_exact(&mut buf).await.expect("read sector 1");
        buf
    });
    assert_eq!(
        fresh, clean,
        "no new storage fault may be injected after the recovery boundary"
    );

    // --- and the damage from before the boundary is still there ---
    let reread = drive(&mut sim, async {
        file.seek(SeekFrom::Start(0)).await.expect("seek");
        let mut buf = vec![0_u8; SECTOR_SIZE];
        file.read_exact(&mut buf).await.expect("re-read sector 0");
        buf
    });
    assert_eq!(
        reread, corrupted,
        "recovery mode must not repair a sector that was already corrupted"
    );
}

/// The block-device surface has its own fault configuration, held per process
/// store, so it needs its own half of the transition.
#[test]
fn block_device_faults_stop_but_planted_corruption_stays() {
    let config = BlockFaultConfig {
        read_corruption_probability: 1.0,
        ..BlockFaultConfig::default()
    };
    let mut sim = SimWorld::new_with_seed(20_260_901);
    sim.set_block_fault_config(config);

    let provider = sim.block_device_provider(ip(1));
    let spec = [RegionSpec {
        name: "data",
        size: 8 * BLOCK_SECTOR_SIZE as u64,
    }];
    let written = vec![0x5A_u8; BLOCK_SECTOR_SIZE];

    let device = Executor::new(20_260_901).block_on(async move {
        let device = provider.create("db", &spec).await.expect("create");
        device.persist().await.expect("persist");
        device
            .write(RegionId(0), 0, &written)
            .await
            .expect("write sector 0");
        device.persist().await.expect("persist");
        device
    });

    let corrupted = Executor::new(20_260_901).block_on({
        let device = device.clone();
        async move {
            let mut buf = vec![0_u8; BLOCK_SECTOR_SIZE];
            device
                .read(RegionId(0), 0, &mut buf)
                .await
                .expect("read sector 0");
            buf
        }
    });
    assert_ne!(
        corrupted,
        vec![0x5A_u8; BLOCK_SECTOR_SIZE],
        "the chaos phase must actually plant a latent fault"
    );

    sim.enter_recovery_mode();

    let (after_recovery, fresh_sector) = Executor::new(20_260_901).block_on({
        let device = device.clone();
        async move {
            let mut old = vec![0_u8; BLOCK_SECTOR_SIZE];
            device
                .read(RegionId(0), 0, &mut old)
                .await
                .expect("re-read sector 0");

            let fresh = vec![0xC3_u8; BLOCK_SECTOR_SIZE];
            device
                .write(RegionId(0), BLOCK_SECTOR_SIZE as u64, &fresh)
                .await
                .expect("write sector 1");
            device.persist().await.expect("persist");
            let mut read_back = vec![0_u8; BLOCK_SECTOR_SIZE];
            device
                .read(RegionId(0), BLOCK_SECTOR_SIZE as u64, &mut read_back)
                .await
                .expect("read sector 1");
            (old, read_back)
        }
    });

    assert_eq!(
        after_recovery, corrupted,
        "a latent fault already planted stays planted, identically on retry"
    );
    assert_eq!(
        fresh_sector,
        vec![0xC3_u8; BLOCK_SECTOR_SIZE],
        "no new block-device corruption may be planted after the boundary"
    );
}

// ===========================================================================
// 3. Finite episodes expire, they are not rewritten
// ===========================================================================

/// A disk-degradation episode carries an expiry. Recovery mode forbids
/// *entering* a new one but leaves one already running to be waited out —
/// rewriting it away would erase timing the system under test has already
/// committed to.
#[test]
fn an_active_disk_throttle_outlives_the_boundary_then_expires() {
    const THROTTLE: Duration = Duration::from_secs(10);

    let mut config = StorageConfiguration::fast_local();
    config.disk_throttle_probability = 1.0;
    config.disk_throttle_duration = THROTTLE;
    config.disk_throttle_iops_multiplier = 1000.0;
    config.disk_throttle_bandwidth_multiplier = 1000.0;
    config.iops = 1_000;
    let mut sim = SimWorld::new_with_seed(20_260_901);
    sim.set_storage_config(config);
    let provider = sim.storage_provider(ip(1));

    let mut file = drive(
        &mut sim,
        provider.open(
            "throttled.bin",
            OpenOptions::new().read(true).write(true).create(true),
        ),
    )
    .expect("open");
    drive(&mut sim, file.write_all(b"tick")).expect("write");

    let expires_at = sim
        .disk_episode_for(ip(1))
        .expect("a throttle episode must be in force before the boundary")
        .expires_at;
    assert!(
        expires_at > sim.current_time(),
        "the episode must still be running when the boundary is crossed"
    );

    sim.enter_recovery_mode();

    assert_eq!(
        sim.disk_episode_for(ip(1))
            .expect("the running episode outlives the boundary")
            .expires_at,
        expires_at,
        "recovery mode must not rewrite an episode's deadline"
    );

    // The episode still bites: a write inside the window pays the throttled
    // IOPS cost, which is a full second at these knobs.
    let before = sim.current_time();
    drive(&mut sim, file.write_all(b"tock")).expect("write");
    assert!(
        sim.current_time().saturating_sub(before) >= Duration::from_millis(500),
        "an episode already in force keeps slowing I/O after the boundary"
    );

    // Wait it out, then keep working: it clears itself and, with the family
    // switched off, no replacement is ever entered.
    let sleep = sim.sleep(THROTTLE * 2);
    drive(&mut sim, sleep).expect("sleep");
    for _ in 0..20 {
        drive(&mut sim, file.write_all(b"tack")).expect("write");
    }
    assert!(
        sim.disk_episode_for(ip(1)).is_none(),
        "the episode expired and no new one was entered after the boundary"
    );
}

// ===========================================================================
// 4. Non-chaos configuration survives
// ===========================================================================

/// Assert an `f64` is exactly `+0.0` (bit-exact, avoiding the float-cmp lint).
fn assert_zero(value: f64, message: &str) {
    assert_eq!(value.to_bits(), 0.0_f64.to_bits(), "{message}, got {value}");
}

/// Recovery mode mutates the network and storage configurations, so the
/// performance characteristics that are *not* chaos have to be shown to
/// survive it: latency shaping, IOPS, bandwidth, throttle multipliers.
#[test]
fn recovery_mode_keeps_non_chaos_configuration() {
    const CONNECT: Duration = Duration::from_millis(7);
    const WRITE: Duration = Duration::from_micros(321);
    const READ: Duration = Duration::from_micros(654);

    let mut network = NetworkConfiguration::fast_local();
    network.connect_latency = LatencyDistribution::Uniform {
        start: CONNECT,
        end: CONNECT,
    };
    network.write_latency = LatencyDistribution::Uniform {
        start: WRITE,
        end: WRITE,
    };
    network.chaos.partition_probability = 1.0;
    network.chaos.clog_probability = 1.0;
    let mut sim = SimWorld::new_with_network_config_and_seed(network, 20_260_901);

    let mut storage = StorageConfiguration::fast_local();
    storage.iops = 1234;
    storage.bandwidth = 5_678_000;
    storage.read_latency = LatencyDistribution::Uniform {
        start: READ,
        end: READ,
    };
    storage.disk_throttle_iops_multiplier = 9.0;
    storage.disk_throttle_bandwidth_multiplier = 11.0;
    storage.read_fault_probability = 1.0;
    sim.set_storage_config(storage);

    sim.enter_recovery_mode();

    sim.with_network_config(|config| {
        assert_eq!(
            config.connect_latency,
            LatencyDistribution::Uniform {
                start: CONNECT,
                end: CONNECT
            },
            "connect latency is deployment shape, not chaos"
        );
        assert_eq!(
            config.write_latency,
            LatencyDistribution::Uniform {
                start: WRITE,
                end: WRITE
            },
            "write latency is deployment shape, not chaos"
        );
        assert_zero(
            config.chaos.partition_probability,
            "partitioning is chaos and must be off",
        );
        assert_zero(
            config.chaos.clog_probability,
            "clogging is chaos and must be off",
        );
    });

    sim.with_storage_config(|config| {
        assert_eq!(config.iops, 1234, "IOPS is disk performance, not chaos");
        assert_eq!(
            config.bandwidth, 5_678_000,
            "bandwidth is disk performance, not chaos"
        );
        assert_eq!(
            config.read_latency,
            LatencyDistribution::Uniform {
                start: READ,
                end: READ
            },
            "read latency is disk performance, not chaos"
        );
        assert_eq!(
            config.disk_throttle_iops_multiplier.to_bits(),
            9.0_f64.to_bits(),
            "an episode still ticking down needs its multipliers"
        );
        assert_eq!(
            config.disk_throttle_bandwidth_multiplier.to_bits(),
            11.0_f64.to_bits()
        );
        assert_zero(
            config.read_fault_probability,
            "read faults are chaos and must be off",
        );
    });
}

/// A pair that already sampled its permanent extra latency keeps it. Zeroing
/// `max_pair_latency` stops *new* pairs from degrading; it does not repair a
/// link that is already slow.
#[test]
fn an_already_degraded_pair_keeps_its_latency() {
    let mut config = NetworkConfiguration::fast_local();
    config.chaos.max_pair_latency = Duration::from_millis(80)..Duration::from_millis(81);
    let mut sim = SimWorld::new_with_network_config_and_seed(config, 20_260_901);

    let server_provider = sim.network_provider(ip(2));
    let listener = drive(&mut sim, server_provider.bind("10.0.1.2:8080")).expect("bind");
    let client_provider = sim.network_provider(ip(1));
    let mut client = drive(&mut sim, client_provider.connect("10.0.1.2:8080")).expect("connect");
    let (mut server, _) = drive(&mut sim, listener.accept()).expect("accept");
    drive(&mut sim, client.write_all(b"warm")).expect("write");
    let mut warm = [0_u8; 4];
    drive(&mut sim, server.read_exact(&mut warm)).expect("read");

    let degraded = sim
        .pair_latency(ip(1), ip(2))
        .expect("the pair sampled its permanent extra latency");
    assert!(degraded >= Duration::from_millis(80));

    sim.enter_recovery_mode();

    assert_eq!(
        sim.pair_latency(ip(1), ip(2)),
        Some(degraded),
        "an already-degraded link is persistent damage, not a fault to heal"
    );
    assert_eq!(
        sim.connection_base_latency(client.connection_id()),
        degraded,
        "the connection still pays the latency its pair already sampled"
    );
}

/// The transition is idempotent and draws no randomness, so calling it cannot
/// shift the RNG stream a replay depends on.
#[test]
fn recovery_mode_is_idempotent_and_consumes_no_randomness() {
    let mut config = NetworkConfiguration::fast_local();
    config.chaos.partition_probability = 1.0;
    let mut sim = SimWorld::new_with_network_config_and_seed(config, 20_260_901);
    sim.partition_pair(ip(1), ip(2), Duration::from_hours(1));

    moonpool_sim::reset_rng_call_count();
    sim.enter_recovery_mode();
    sim.enter_recovery_mode();
    sim.enter_recovery_mode();

    assert_eq!(
        moonpool_sim::rng_call_count(),
        0,
        "the recovery transition must not consume the simulation RNG"
    );
    assert!(!sim.is_partitioned(ip(1), ip(2)));
    assert!(sim.is_in_recovery_mode());
}

/// Clock drift is switched off at the boundary, but a reading already handed
/// out must never be walked back — rewinding a clock is a worse fault than the
/// drift it replaces.
#[test]
fn disabling_clock_drift_never_rewinds_the_clock() {
    let mut config = NetworkConfiguration::fast_local();
    config.chaos.clock_drift_enabled = true;
    config.chaos.clock_drift_max = Duration::from_millis(100);
    let mut sim = SimWorld::new_with_network_config_and_seed(config, 20_260_901);

    let mut drifted = Duration::ZERO;
    for _ in 0..50 {
        drifted = sim.timer();
    }
    assert!(
        drifted > sim.now(),
        "the clock must actually drift ahead, or the test proves nothing"
    );

    sim.enter_recovery_mode();

    assert!(
        sim.timer() >= drifted,
        "the timer must never read below a value already handed out"
    );
}

// ===========================================================================
// 5. The runner crosses the boundary for real
// ===========================================================================

struct EchoProcess;

#[async_trait]
impl Process for EchoProcess {
    fn name(&self) -> &'static str {
        "echo"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        loop {
            let accepted = moonpool_sim::select! {
                biased;
                r = listener.accept() => r,
                () = ctx.shutdown().cancelled() => return Ok(()),
            };
            if let Ok((mut stream, _)) = accepted {
                let mut buf = [0_u8; 16];
                if let Ok(n) = stream.read(&mut buf).await
                    && n > 0
                {
                    let _ = stream.write_all(&buf[..n]).await;
                }
            }
        }
    }
}

/// Cuts the workload off from the server for the whole chaos phase, using the
/// hour-long cut `FaultContext::partition` installs, then parks until the
/// chaos token is cancelled.
struct CutEverything;

#[async_trait]
impl FaultInjector for CutEverything {
    fn name(&self) -> &'static str {
        "cut-everything"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        for process in ctx.process_ips() {
            ctx.partition("10.0.0.1", process)?;
        }
        ctx.chaos_shutdown().cancelled().await;
        Ok(())
    }
}

/// The request is issued during the cut and can only complete once the cutoff
/// heals it. If the boundary stopped healing partitions, this workload would
/// hang and the run-time budget would fail the seed.
struct RequestAcrossTheCut;

#[async_trait]
impl Workload for RequestAcrossTheCut {
    fn name(&self) -> &'static str {
        "request-across-the-cut"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let target = ctx.topology().all_process_ips()[0].clone();
        // Retry on a timer, the way a real client would. Every attempt before
        // the cutoff times out inside the cut; the first attempt after it must
        // succeed. The retries also keep virtual time moving, so the cutoff is
        // reached on schedule instead of being jumped over.
        loop {
            let attempt = async {
                let mut stream = ctx.network().connect(&target).await?;
                stream
                    .write_all(b"ping")
                    .await
                    .map_err(|error| SimulationError::InvalidState(error.to_string()))?;
                let mut buf = [0_u8; 4];
                stream
                    .read_exact(&mut buf)
                    .await
                    .map_err(|error| SimulationError::InvalidState(error.to_string()))?;
                Ok::<[u8; 4], SimulationError>(buf)
            };
            if let Ok(Ok(reply)) = ctx
                .time()
                .timeout(Duration::from_millis(500), attempt)
                .await
            {
                assert_eq!(&reply, b"ping", "the echo server replies with the request");
                return Ok(());
            }
        }
    }
}

#[test]
fn a_campaign_heals_its_partitions_at_the_chaos_cutoff() {
    let report = SimulationBuilder::new()
        .processes(1, || Box::new(EchoProcess))
        .workload(RequestAcrossTheCut)
        .fault_factory(|| Box::new(CutEverything))
        // The cut is the only fault under test: silence the default network
        // chaos families so a connect failure cannot muddy the result.
        .network_fault_mask(NetworkFaultMask::none())
        .chaos_duration(Duration::from_secs(5))
        // The cut lasts an hour; only the cutoff can release the request in
        // time, so a stranded request trips this budget instead of hanging.
        .run_time_budget(Duration::from_mins(2))
        .set_iterations(3)
        .set_debug_seeds(vec![1, 2, 3])
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "the request must complete once the cutoff heals the cut:\n{report}"
    );
}
