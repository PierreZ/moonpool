//! Known-answer tests of the CPU model (design: paros #424, section 4.2).
//!
//! Each test drives a raw world and the executor by hand, with exact costs,
//! so every expected number is computed, not measured.

use std::{
    net::{IpAddr, Ipv4Addr},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_core::{
    NetworkProvider, OpenOptions, StorageFile, StorageProvider, SystemProvider, SystemSample,
    TcpListenerTrait, TimeProvider,
};

use super::{CpuModel, Slowness};
use crate::{
    LatencyDistribution, NetworkConfiguration, SimSystemProvider,
    executor::{Executor, JoinHandle, spawn, spawn_owned, until_stalled},
    sim::{SimWorld, rng::sim_random_f64},
};

const SERVER: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 1, 1));
const OTHER: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 1, 2));
const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

type Shared<T> = Arc<Mutex<T>>;

fn lock<T>(shared: &Shared<T>) -> std::sync::MutexGuard<'_, T> {
    shared.lock().expect("Mutex poisoned: prior task panicked")
}

/// A network that adds no latency anywhere, so a round trip is pure CPU.
fn zero_latency() -> NetworkConfiguration {
    let zero = LatencyDistribution::Uniform {
        start: Duration::ZERO,
        end: Duration::ZERO,
    };
    NetworkConfiguration {
        bind_latency: zero.clone(),
        accept_latency: zero.clone(),
        connect_latency: zero.clone(),
        write_latency: zero,
        ..NetworkConfiguration::fast_local()
    }
}

/// A world with `model` on.
fn world(seed: u64, model: CpuModel) -> SimWorld {
    let sim = SimWorld::new_with_network_config_and_seed(zero_latency(), seed);
    sim.set_cpu_model(Some(model));
    sim
}

/// Drive the world and the tasks until no event is left.
async fn run(sim: &mut SimWorld) {
    loop {
        until_stalled().await;
        if !sim.has_pending_events() {
            break;
        }
        sim.step();
    }
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).expect("test durations fit in u64 nanoseconds")
}

/// 1. An idle machine slowed ×1000 answers one ping in exactly
///    `1000 × poll_cost`: the read and the reply are one poll.
#[test]
fn an_idle_slow_machine_answers_a_ping_in_its_slowed_poll_cost() {
    let poll_cost = Duration::from_micros(2);
    let mut sim = world(1, CpuModel::new(poll_cost, 0, 0));
    sim.set_slowness(
        SERVER,
        Slowness {
            cpu: 1000,
            ..Slowness::HEALTHY
        },
    );
    let round_trip: Shared<Option<Duration>> = Arc::default();
    let observed = Arc::clone(&round_trip);
    let server_net = sim.network_provider(SERVER);
    let client_net = sim.network_provider(CLIENT);
    let time = sim.time_provider();
    Executor::new(1).block_on(async {
        let _server = spawn_owned("server", SERVER, async move {
            let listener = server_net.bind("10.0.1.1:7").await.expect("bind");
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = [0_u8; 4];
            loop {
                if stream.read_exact(&mut buf).await.is_err() {
                    return;
                }
                if stream.write_all(&buf).await.is_err() {
                    return;
                }
            }
        });
        let _client = spawn("client", async move {
            time.sleep(Duration::from_secs(1)).await.expect("sleep");
            let mut stream = client_net.connect("10.0.1.1:7").await.expect("connect");
            // Let the server go idle before the ping.
            time.sleep(Duration::from_secs(1)).await.expect("sleep");
            let sent = time.now();
            stream.write_all(b"ping").await.expect("write");
            let mut buf = [0_u8; 4];
            stream.read_exact(&mut buf).await.expect("read");
            assert_eq!(&buf, b"ping");
            *lock(&observed) = Some(time.now().saturating_sub(sent));
        });
        run(&mut sim).await;
    });
    assert_eq!(*lock(&round_trip), Some(poll_cost * 1000));
}

/// 2. N requests at t = 0 on one core, each one poll of cost `c`: they are
///    served at `c, 2c, …, Nc`, and the core is 100% busy over `[0, Nc]`.
#[test]
fn n_requests_at_once_are_served_one_core_at_a_time() {
    const N: u32 = 8;
    let cost = Duration::from_micros(3);
    let mut sim = world(2, CpuModel::new(cost, 0, 0));
    let served: Shared<Vec<Duration>> = Arc::default();
    let busy: Shared<Vec<(Duration, SystemSample)>> = Arc::default();
    let weak = sim.downgrade();
    Executor::new(2).block_on(async {
        for _ in 0..N {
            let served = Arc::clone(&served);
            let weak = weak.clone();
            let _request = spawn_owned("request", SERVER, async move {
                // The reply's instant: the local clock after the poll's cost.
                lock(&served).push(weak.now().expect("world alive"));
            });
        }
        let observer_busy = Arc::clone(&busy);
        let observer_weak = weak.clone();
        let _observer = spawn("observer", async move {
            let world = observer_weak.upgrade().expect("world alive");
            for _ in 0..N {
                world.sleep(cost).await.expect("sleep");
                let sample = world.system_sample(SERVER);
                lock(&observer_busy).push((world.now(), sample));
            }
        });
        run(&mut sim).await;
    });
    let mut served = lock(&served).clone();
    served.sort_unstable();
    let expected: Vec<Duration> = (1..=N).map(|k| cost * k).collect();
    assert_eq!(served, expected);
    for (at, sample) in lock(&busy).iter() {
        assert_eq!(sample.process_cpu, *at, "100% busy up to {at:?}");
        assert_eq!(sample.run_loop_busy, Some(*at));
        assert_eq!(sample.machine_busy, *at);
    }
    assert_eq!(lock(&busy).last().map(|(at, _)| *at), Some(cost * N));
}

/// 3. Poisson arrivals at rate `L`, each one poll of cost `c`: the core's
///    utilization is close to `L × c`.
#[test]
fn poisson_arrivals_use_the_core_at_their_offered_load() {
    let cost = Duration::from_micros(5);
    let rate_per_sec = 120_000.0_f64; // L × c = 0.6
    let horizon = Duration::from_millis(100);
    let mut sim = world(3, CpuModel::new(cost, 0, 0));
    let utilization: Shared<Option<f64>> = Arc::default();
    let weak = sim.downgrade();
    let observed = Arc::clone(&utilization);
    Executor::new(3).block_on(async {
        let _generator = spawn("generator", async move {
            let world = weak.upgrade().expect("world alive");
            loop {
                let gap = -(1.0 - sim_random_f64()).ln() / rate_per_sec;
                world
                    .sleep(Duration::from_secs_f64(gap))
                    .await
                    .expect("sleep");
                if world.now() >= horizon {
                    break;
                }
                let _arrival = spawn_owned("arrival", SERVER, async {});
            }
            let busy = world.cpu_busy(SERVER).as_secs_f64();
            *lock(&observed) = Some(busy / world.now().as_secs_f64());
        });
        run(&mut sim).await;
    });
    let utilization = lock(&utilization).expect("the generator finished");
    assert!(
        (utilization - 0.6).abs() < 0.03,
        "utilization {utilization} is not close to L × c = 0.6"
    );
}

/// A busy process: work loops with charges, sends nothing, writes to its
/// disk, and samples its own counters; returns every sample it took and
/// every sample the harness took from outside.
fn sampled_run(seed: u64) -> (Vec<SystemSample>, Vec<SystemSample>) {
    let mut sim = world(seed, CpuModel::new(Duration::from_micros(4), 1_000, 2_000));
    sim.set_slowness(
        SERVER,
        Slowness {
            cpu: 3,
            disk: 2,
            network: 1,
        },
    );
    let own: Shared<Vec<SystemSample>> = Arc::default();
    let outside: Shared<Vec<SystemSample>> = Arc::default();
    let weak = sim.downgrade();
    let storage = sim.storage_provider(SERVER);
    let time = sim.time_provider();
    Executor::new(seed).block_on(async {
        let worker_time = time.clone();
        let _worker = spawn_owned("worker", SERVER, async move {
            let mut file = storage
                .open("load.dat", OpenOptions::create_write())
                .await
                .expect("open");
            for round in 0..200_u32 {
                let pause = Duration::from_micros(u64::from(round % 7) * 50);
                worker_time.sleep(pause).await.expect("sleep");
                moonpool_buggify::cpu_charge(Duration::from_micros(u64::from(round % 5) * 30));
                let bytes = vec![0_u8; 512 * usize::try_from(round % 4 + 1).expect("small")];
                file.write_all(&bytes).await.expect("write");
                if round % 10 == 0 {
                    file.sync_all().await.expect("sync");
                }
            }
        });
        let system = SimSystemProvider::new(weak.clone(), SERVER);
        let sampler_own = Arc::clone(&own);
        let sampler_time = time.clone();
        let _sampler = spawn_owned("sampler", SERVER, async move {
            for _ in 0..60 {
                sampler_time
                    .sleep(Duration::from_micros(700))
                    .await
                    .expect("sleep");
                lock(&sampler_own).push(system.sample("").expect("sample"));
            }
        });
        let harness_outside = Arc::clone(&outside);
        let _harness = spawn("harness", async move {
            let world = weak.upgrade().expect("world alive");
            for _ in 0..60 {
                time.sleep(Duration::from_micros(530)).await.expect("sleep");
                lock(&harness_outside).push(world.system_sample(SERVER));
            }
        });
        run(&mut sim).await;
    });
    let own = lock(&own).clone();
    let outside = lock(&outside).clone();
    (own, outside)
}

/// The FDB ratios of every window between two consecutive samples are in
/// range: `0 ≤ cpu_cores ≤ cores` and `0 ≤ disk_busy ≤ 1`.
fn assert_windows_in_range(samples: &[SystemSample]) {
    assert!(samples.len() > 10, "enough samples to judge");
    for pair in samples.windows(2) {
        let (before, after) = (&pair[0], &pair[1]);
        let elapsed = nanos(after.at.checked_sub(before.at).expect("time goes on"));
        let cpu = nanos(
            after
                .process_cpu
                .checked_sub(before.process_cpu)
                .expect("cpu counter never goes back"),
        );
        assert!(
            cpu <= elapsed * u64::from(after.cores),
            "cpu_cores {cpu}/{elapsed} above {} core(s)",
            after.cores
        );
        let disk = |sample: &SystemSample| nanos(sample.disk.as_ref().expect("disk").io_ticks);
        let io = disk(after)
            .checked_sub(disk(before))
            .expect("io_ticks never goes back");
        assert!(io <= elapsed, "disk_busy {io}/{elapsed} above 1");
    }
}

/// 4. At every sample, from inside the process and from outside, every
///    window's `cpu_cores` and `disk_busy` are in range.
#[test]
fn every_sample_window_is_in_range() {
    let (own, outside) = sampled_run(4);
    assert_windows_in_range(&own);
    assert_windows_in_range(&outside);
    let last = own.last().expect("samples");
    assert!(
        last.process_cpu > Duration::ZERO,
        "the CPU model counted work"
    );
}

/// 5. The same seed twice gives the same samples, bit for bit.
#[test]
fn the_same_seed_gives_identical_samples() {
    assert_eq!(sampled_run(5), sampled_run(5));
}

/// 6. A process killed with parked tasks restarts with no parked task and
///    zero counters.
#[test]
fn a_kill_drops_the_parked_tasks_and_resets_the_slot() {
    let cost = Duration::from_micros(10);
    let mut sim = world(6, CpuModel::new(cost, 0, 0));
    Executor::new(6).block_on(async {
        let handles: Vec<JoinHandle<()>> = (0..3)
            .map(|_| spawn_owned("doomed", SERVER, async {}))
            .collect();
        // One poll runs; the other two meet a busy core and park.
        until_stalled().await;
        {
            let inner = sim.inner.read();
            assert_eq!(inner.cpu.parked.get(&SERVER).map(Vec::len), Some(2));
        }
        assert_eq!(sim.inner.read().cpu_busy(SERVER, cost), cost);

        sim.reset_cpu_owner(SERVER);
        {
            let inner = sim.inner.read();
            assert!(!inner.cpu.parked.contains_key(&SERVER), "no parked task");
            assert!(!inner.cpu.slots.contains_key(&SERVER), "a fresh slot");
        }
        assert_eq!(sim.cpu_busy(SERVER), Duration::ZERO);
        let mut cancelled = 0;
        for handle in handles {
            if handle.await.is_err() {
                cancelled += 1;
            }
        }
        assert_eq!(cancelled, 2, "the parked tasks died with the process");

        // The next boot runs at once on a free core.
        let weak = sim.downgrade();
        let reboot = spawn_owned(
            "reboot",
            SERVER,
            async move { weak.now().expect("world alive") },
        );
        until_stalled().await;
        assert_eq!(reboot.await.expect("rebooted task ran"), cost);
        run(&mut sim).await;
    });
}

/// 7. The same work on a machine slowed ×k costs exactly k times the CPU of
///    the healthy machine: polls, explicit charges and bytes alike.
#[test]
fn a_slow_machine_pays_its_factor_for_the_same_work() {
    const FACTOR: u32 = 7;
    let mut sim = world(7, CpuModel::new(Duration::from_micros(1), 3_000, 2_000));
    sim.set_slowness(
        OTHER,
        Slowness {
            cpu: FACTOR,
            ..Slowness::HEALTHY
        },
    );
    let busy: Shared<Option<(Duration, Duration)>> = Arc::default();
    let observed = Arc::clone(&busy);
    let weak = sim.downgrade();
    Executor::new(7).block_on(async {
        // Read the counters once both machines are idle again: a sample
        // never counts CPU time after its instant.
        let _observer = spawn("observer", async move {
            let world = weak.upgrade().expect("world alive");
            world.sleep(Duration::from_secs(1)).await.expect("sleep");
            *lock(&observed) = Some((world.cpu_busy(SERVER), world.cpu_busy(OTHER)));
        });
        for ip in [SERVER, OTHER] {
            let storage = sim.storage_provider(ip);
            let time = sim.time_provider();
            let _work = spawn_owned("work", ip, async move {
                let mut file = storage
                    .open("work.dat", OpenOptions::create_write())
                    .await
                    .expect("open");
                for round in 0..20_u64 {
                    moonpool_buggify::cpu_charge(Duration::from_nanos(700 * round));
                    file.write_all(&[7_u8; 1_000]).await.expect("write");
                    time.sleep(Duration::from_millis(1)).await.expect("sleep");
                }
            });
        }
        run(&mut sim).await;
    });
    let (healthy, slow) = lock(&busy).expect("observed");
    assert!(healthy > Duration::ZERO);
    assert_eq!(slow, healthy * FACTOR);
}

/// The gray failure's disk and network factors multiply their latencies
/// exactly, with the CPU model off: a write on a disk slowed ×5 takes five
/// times as long, and a ping to a machine slowed ×4 takes four times.
#[test]
fn gray_factors_multiply_disk_and_network_latency() {
    let latency = Duration::from_micros(10);
    let fixed = LatencyDistribution::Uniform {
        start: latency,
        end: latency,
    };
    let mut sim = SimWorld::new_with_network_config_and_seed(
        NetworkConfiguration {
            write_latency: fixed.clone(),
            ..zero_latency()
        },
        8,
    );
    sim.set_storage_config(crate::StorageConfiguration {
        write_latency: fixed,
        ..crate::StorageConfiguration::fast_local()
    });
    sim.set_slowness(
        OTHER,
        Slowness {
            disk: 5,
            network: 4,
            ..Slowness::HEALTHY
        },
    );
    let writes: Shared<Vec<(IpAddr, Duration)>> = Arc::default();
    let round_trip: Shared<Option<Duration>> = Arc::default();
    let observed = Arc::clone(&round_trip);
    let server_net = sim.network_provider(OTHER);
    let client_net = sim.network_provider(CLIENT);
    let time = sim.time_provider();
    Executor::new(8).block_on(async {
        for ip in [SERVER, OTHER] {
            let storage = sim.storage_provider(ip);
            let time = sim.time_provider();
            let writes = Arc::clone(&writes);
            let _write = spawn_owned("write", ip, async move {
                let mut file = storage
                    .open("gray.dat", OpenOptions::create_write())
                    .await
                    .expect("open");
                let started = time.now();
                file.write_all(&[1_u8; 64]).await.expect("write");
                lock(&writes).push((ip, time.now().saturating_sub(started)));
            });
        }
        let _server = spawn_owned("server", OTHER, async move {
            let listener = server_net.bind("10.0.1.2:7").await.expect("bind");
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = [0_u8; 4];
            while stream.read_exact(&mut buf).await.is_ok() {
                if stream.write_all(&buf).await.is_err() {
                    return;
                }
            }
        });
        let _client = spawn("client", async move {
            time.sleep(Duration::from_secs(1)).await.expect("sleep");
            let mut stream = client_net.connect("10.0.1.2:7").await.expect("connect");
            time.sleep(Duration::from_secs(1)).await.expect("sleep");
            let sent = time.now();
            stream.write_all(b"ping").await.expect("write");
            let mut buf = [0_u8; 4];
            stream.read_exact(&mut buf).await.expect("read");
            *lock(&observed) = Some(time.now().saturating_sub(sent));
        });
        run(&mut sim).await;
    });
    let writes = lock(&writes).clone();
    let took = |ip: IpAddr| {
        writes
            .iter()
            .find(|(at, _)| *at == ip)
            .map(|(_, took)| *took)
            .expect("the write finished")
    };
    assert!(took(SERVER) > Duration::ZERO);
    assert_eq!(took(OTHER), took(SERVER) * 5);
    assert_eq!(*lock(&round_trip), Some(latency * 4 * 2));
}
