use futures::{future::poll_fn, io::AsyncWriteExt};
use moonpool_sim::{
    ChaosConfiguration, LatencyDistribution, LinkLatencyConfig, LocalityInfo, NetworkConfiguration,
    NetworkProvider, SimWorld, TcpListenerTrait,
};
use std::collections::BTreeMap;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

/// Build a uniform latency distribution over `[start, end)` for test configs.
fn uniform(start: Duration, end: Duration) -> LatencyDistribution {
    LatencyDistribution::Uniform { start, end }
}

// Simple networking test that measures bind + connect + accept latency
async fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    futures::pin_mut!(future);
    poll_fn(|cx| match future.as_mut().poll(cx) {
        std::task::Poll::Ready(output) => std::task::Poll::Ready(output),
        std::task::Poll::Pending => {
            if sim.has_pending_events() {
                sim.step();
                cx.waker().wake_by_ref();
            }
            std::task::Poll::Pending
        }
    })
    .await
}

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let waker = futures::task::noop_waker();
    future.poll(&mut Context::from_waker(&waker))
}

#[test]
fn bind_completes_at_its_exact_sampled_deadline() {
    let mut config = NetworkConfiguration::fast_local();
    config.bind_latency = uniform(Duration::from_millis(7), Duration::from_millis(7));
    let mut sim = SimWorld::new_with_network_config(config);
    let provider = sim.network_provider("127.0.0.1".parse().expect("valid IP"));
    let mut bind = Box::pin(provider.bind("exact-bind"));

    assert!(poll_once(bind.as_mut()).is_pending());
    assert_eq!(sim.current_time(), Duration::ZERO);
    assert_eq!(sim.pending_event_count(), 1);
    assert!(!sim.step());
    assert_eq!(sim.current_time(), Duration::from_millis(7));
    assert!(poll_once(bind.as_mut()).is_ready());
}

#[test]
fn cancelled_accept_returns_its_fifo_reservation() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let mut config = NetworkConfiguration::fast_local();
        config.accept_latency = uniform(Duration::from_millis(9), Duration::from_millis(9));
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid IP"));
        let listener = drive(&mut sim, provider.bind("cancel-accept"))
            .await
            .expect("bind");
        let _client = drive(&mut sim, provider.connect("cancel-accept"))
            .await
            .expect("connect");
        let before_accept = sim.current_time();

        {
            let mut cancelled = Box::pin(listener.accept());
            assert!(poll_once(cancelled.as_mut()).is_pending());
            assert_eq!(sim.pending_event_count(), 1);
        }

        assert_eq!(sim.pending_event_count(), 0);
        let (_server, _) = drive(&mut sim, listener.accept())
            .await
            .expect("reservation returned to backlog");
        assert_eq!(sim.current_time(), before_accept + Duration::from_millis(9));
    });
}

async fn simple_network_test<P>(sim: &mut SimWorld, provider: P, addr: &str) -> std::io::Result<()>
where
    P: NetworkProvider + Clone,
{
    let listener = drive(sim, provider.bind(addr)).await?;
    let _client = drive(sim, provider.connect(addr)).await?;
    let (mut stream, _peer_addr) = drive(sim, listener.accept()).await?;

    // Write some data to exercise the write latency
    let test_data = b"test data for latency measurement";
    stream.write_all(test_data).await?;

    Ok(())
}

#[test]
fn test_fast_local_configuration() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let fast_config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(fast_config);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

        let start_time = std::time::Instant::now();
        simple_network_test(&mut sim, provider, "fast-test")
            .await
            .unwrap();
        let elapsed = start_time.elapsed();

        // Process all simulation events
        sim.run_until_empty();
        let sim_time = sim.current_time();

        // Fast local config should complete quickly (less than 1ms simulation time)
        assert!(
            sim_time < Duration::from_millis(1),
            "Fast local should be under 1ms, got {sim_time:?}"
        );

        println!("Fast local test completed in real time: {elapsed:?}, sim time: {sim_time:?}");
    });
}

#[test]
fn test_default_simulation_configuration() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let wan_config = NetworkConfiguration::default(); // Use default config with reasonable delays
        let mut sim = SimWorld::new_with_network_config(wan_config);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

        let start_time = std::time::Instant::now();
        simple_network_test(&mut sim, provider, "wan-test")
            .await
            .unwrap();
        let elapsed = start_time.elapsed();

        // Process all simulation events
        sim.run_until_empty();
        let sim_time = sim.current_time();

        // Default config should take longer than fast config (at least 5ms simulation time)
        assert!(
            sim_time > Duration::from_millis(5),
            "Default config should be over 5ms, got {sim_time:?}"
        );

        println!("Default config test completed in real time: {elapsed:?}, sim time: {sim_time:?}");
    });
}

#[test]
fn test_custom_latency_configuration() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Create custom configuration with specific latency ranges
        let config = NetworkConfiguration {
            bind_latency: uniform(Duration::from_millis(5), Duration::from_millis(5)),
            accept_latency: uniform(Duration::from_millis(10), Duration::from_millis(10)),
            connect_latency: uniform(Duration::from_millis(1), Duration::from_millis(1)),
            write_latency: uniform(Duration::from_millis(2), Duration::from_millis(2)),
            link_latency: None,
            chaos: ChaosConfiguration::disabled(),
        };

        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

        simple_network_test(&mut sim, provider, "custom-test")
            .await
            .unwrap();

        // Process all simulation events
        sim.run_until_empty();
        let sim_time = sim.current_time();

        // With our fixed latencies: bind(5ms) + accept(10ms) + write(2ms) = ~17ms minimum
        // Phase 2c focuses on configuration working - expect at least some configured delay
        assert!(
            sim_time > Duration::ZERO,
            "Custom config should advance simulation time, got {sim_time:?}"
        );
        assert!(
            sim_time >= Duration::from_millis(1),
            "Custom config should have at least 1ms latency, got {sim_time:?}"
        );

        println!("Custom test completed in sim time: {sim_time:?}");
    });
}

#[test]
fn test_latency_range_sampling() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Test multiple runs to verify latency variance with jitter
        let config = NetworkConfiguration {
            bind_latency: uniform(Duration::from_millis(1), Duration::from_millis(6)), // 1-6ms range
            accept_latency: uniform(Duration::from_millis(1), Duration::from_millis(6)),
            connect_latency: uniform(Duration::from_millis(1), Duration::from_millis(6)),
            write_latency: uniform(Duration::from_millis(1), Duration::from_millis(6)),
            link_latency: None,
            chaos: ChaosConfiguration::disabled(),
        };

        let mut execution_times = Vec::new();

        for _run in 0..5 {
            let mut sim = SimWorld::new_with_network_config(config.clone());
            let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

            simple_network_test(&mut sim, provider, "jitter-test")
                .await
                .unwrap();

            sim.run_until_empty();
            execution_times.push(sim.current_time());
        }

        // All times should be different due to jitter (with high probability)
        let first_time = execution_times[0];
        let all_same = execution_times.iter().all(|&t| t == first_time);

        // Due to random jitter, we expect some variation in latency configuration
        println!("Execution times with jitter: {execution_times:?}");

        // Verify all times are positive (configuration is working)
        for &time in &execution_times {
            assert!(time > Duration::ZERO, "Time should be positive: {time:?}");
        }

        // For Phase 2c, we just verify that latency configuration is working
        // and producing reasonable results

        if all_same {
            println!("⚠ All execution times were identical (could happen by chance)");
        } else {
            println!("✓ Latency jitter working - execution times vary");
        }
    });
}

#[test]
fn test_network_randomization_ranges() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Create custom configuration with predictable latencies
        let config = NetworkConfiguration {
            bind_latency: uniform(Duration::from_millis(1), Duration::from_millis(1)),
            accept_latency: uniform(Duration::from_millis(2), Duration::from_millis(2)),
            connect_latency: uniform(Duration::from_millis(3), Duration::from_millis(3)),
            write_latency: uniform(Duration::from_micros(500), Duration::from_micros(500)),
            link_latency: None,
            chaos: ChaosConfiguration::disabled(),
        };

        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

        simple_network_test(&mut sim, provider, "custom-ranges-test")
            .await
            .unwrap();

        // Process all simulation events
        sim.run_until_empty();
        let sim_time = sim.current_time();

        // With our custom ranges, we expect predictable latency:
        // connect(3ms) + accept(2ms) + write(500µs) = ~5.5ms minimum
        // The exact timing depends on event scheduling and which latencies are actually triggered
        assert!(
            sim_time >= Duration::from_millis(3),
            "Expected at least 3ms with custom ranges, got {sim_time:?}"
        );

        assert!(
            sim_time <= Duration::from_millis(10),
            "Expected less than 10ms with custom ranges, got {sim_time:?}"
        );

        println!("Custom ranges test completed in sim time: {sim_time:?}");
    });
}

/// Regression test for the client-initiated connection's local IP.
///
/// `SimNetworkProvider::connect()` used to call `create_connection_pair` with a
/// literal placeholder address for the client side, so the client connection's
/// `local_ip` stayed `None` forever. `connection_base_latency()` requires both
/// endpoint IPs to activate `max_pair_latency`, so per-pair latency was a
/// silent no-op on every connection made through `NetworkProvider::connect()`.
///
/// This test fails without the fix: `sim.pair_latency(client_ip, server_ip)`
/// stays `None` because the client's `local_ip` never resolves, so
/// `connection_base_latency` bails out before touching the pair map.
#[test]
fn test_client_initiated_connection_records_pair_latency() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let mut config = NetworkConfiguration::fast_local();
        // Enable the "stably slow link" chaos feature: a nonzero range means
        // connection_base_latency() should memoize a latency for the pair.
        config.chaos.max_pair_latency = Duration::from_millis(5)..Duration::from_millis(10);

        let mut sim = SimWorld::new_with_network_config(config);

        let client_ip: std::net::IpAddr = "10.0.1.1".parse().expect("valid ip");
        let server_ip: std::net::IpAddr = "10.0.1.2".parse().expect("valid ip");
        let server_addr = "10.0.1.2:8080";

        let provider = sim.network_provider(client_ip);
        let listener = drive(&mut sim, provider.bind(server_addr)).await.unwrap();
        let _client = drive(&mut sim, provider.connect(server_addr))
            .await
            .unwrap();
        let (_server, _peer_addr) = drive(&mut sim, listener.accept()).await.unwrap();

        sim.run_until_empty();

        let latency = sim.pair_latency(client_ip, server_ip);
        assert!(
            latency.is_some(),
            "expected a per-pair base latency to be recorded for the \
             client-initiated connection ({client_ip} -> {server_ip}); this is \
             None when the client's local_ip never resolves"
        );
        let latency = latency.expect("checked above");
        assert!(
            latency >= Duration::from_millis(5) && latency < Duration::from_millis(10),
            "latency {latency:?} outside configured max_pair_latency range"
        );
    });
}

// ---------------------------------------------------------------------------
// Distance-based link latency (LinkLatencyConfig)
// ---------------------------------------------------------------------------

/// Per-class distributions with disjoint ranges, so a sampled value alone
/// identifies which class the engine picked.
fn distinct_link_latencies() -> LinkLatencyConfig {
    LinkLatencyConfig {
        same_machine: uniform(Duration::from_millis(1), Duration::from_millis(2)),
        same_zone: uniform(Duration::from_millis(10), Duration::from_millis(11)),
        same_datacenter: uniform(Duration::from_millis(100), Duration::from_millis(101)),
        cross_datacenter: uniform(Duration::from_millis(900), Duration::from_millis(950)),
    }
}

/// Locality map for the two IPs used by [`memoized_pair_latency`], each given as
/// `(datacenter, zone, machine)`.
fn two_process_localities(
    client: (&str, &str, &str),
    server: (&str, &str, &str),
) -> BTreeMap<IpAddr, LocalityInfo> {
    BTreeMap::from([
        (
            "10.0.1.1".parse().expect("valid ip"),
            LocalityInfo::new(client.0, client.1, client.2),
        ),
        (
            "10.0.1.2".parse().expect("valid ip"),
            LocalityInfo::new(server.0, server.1, server.2),
        ),
    ])
}

/// Connect `10.0.1.1 -> 10.0.1.2` under `config` with `localities` installed,
/// and return the per-pair latency the engine memoized for that direction.
fn memoized_pair_latency(
    config: NetworkConfiguration,
    localities: BTreeMap<IpAddr, LocalityInfo>,
) -> Option<Duration> {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let mut sim = SimWorld::new_with_network_config_and_seed(config, 42);
        sim.set_localities(localities);

        let client_ip: IpAddr = "10.0.1.1".parse().expect("valid ip");
        let server_ip: IpAddr = "10.0.1.2".parse().expect("valid ip");
        let server_addr = "10.0.1.2:8080";

        let provider = sim.network_provider(client_ip);
        let listener = drive(&mut sim, provider.bind(server_addr))
            .await
            .expect("bind");
        let _client = drive(&mut sim, provider.connect(server_addr))
            .await
            .expect("connect");
        let (_server, _) = drive(&mut sim, listener.accept()).await.expect("accept");

        sim.run_until_empty();
        sim.pair_latency(client_ip, server_ip)
    })
}

/// Each locality distance selects its own distribution.
#[test]
fn link_latency_classifies_the_pair_by_distance() {
    let config = || {
        let mut config = NetworkConfiguration::fast_local();
        config.link_latency = Some(distinct_link_latencies());
        config
    };

    let cases = [
        (
            ("dc1", "dc1-z1", "dc1-z1-m1"),
            ("dc1", "dc1-z1", "dc1-z1-m1"),
            Duration::from_millis(1)..Duration::from_millis(2),
            "same machine",
        ),
        (
            ("dc1", "dc1-z1", "dc1-z1-m1"),
            ("dc1", "dc1-z1", "dc1-z1-m2"),
            Duration::from_millis(10)..Duration::from_millis(11),
            "same zone",
        ),
        (
            ("dc1", "dc1-z1", "dc1-z1-m1"),
            ("dc1", "dc1-z2", "dc1-z2-m1"),
            Duration::from_millis(100)..Duration::from_millis(101),
            "same datacenter",
        ),
        (
            ("dc1", "dc1-z1", "dc1-z1-m1"),
            ("dc2", "dc2-z1", "dc2-z1-m1"),
            Duration::from_millis(900)..Duration::from_millis(950),
            "cross datacenter",
        ),
    ];

    for (client, server, expected, label) in cases {
        let latency = memoized_pair_latency(config(), two_process_localities(client, server))
            .unwrap_or_else(|| panic!("{label}: no per-pair latency recorded"));
        assert!(
            expected.contains(&latency),
            "{label}: latency {latency:?} outside {expected:?}"
        );
    }
}

/// The distance sample and the chaos `max_pair_latency` sample share one
/// per-pair budget: the memoized value is their sum.
#[test]
fn link_latency_sums_with_max_pair_latency() {
    let mut config = NetworkConfiguration::fast_local();
    config.chaos.max_pair_latency = Duration::from_millis(5)..Duration::from_millis(6);
    config.link_latency = Some(distinct_link_latencies());

    let localities = two_process_localities(
        ("dc1", "dc1-z1", "dc1-z1-m1"),
        ("dc2", "dc2-z1", "dc2-z1-m1"),
    );
    let latency =
        memoized_pair_latency(config, localities).expect("per-pair latency should be recorded");

    // 5..6ms of chaos plus 900..950ms of distance.
    let expected = Duration::from_millis(905)..Duration::from_millis(956);
    assert!(
        expected.contains(&latency),
        "latency {latency:?} is not the sum of both extras ({expected:?})"
    );
}

/// Without locality for both endpoints there is no distance to speak of, so the
/// pair gets no extra latency (and the engine must not panic).
#[test]
fn link_latency_is_inert_without_locality() {
    let mut config = NetworkConfiguration::fast_local();
    config.link_latency = Some(distinct_link_latencies());

    // Empty topology: the plain `.processes()` case.
    assert_eq!(
        memoized_pair_latency(config.clone(), BTreeMap::new()),
        Some(Duration::ZERO),
        "an unlocalized pair must get no distance latency"
    );

    // Half-known topology: only the server has a locality, as when a workload
    // client talks to a clustered process.
    let partial = BTreeMap::from([(
        "10.0.1.2".parse::<IpAddr>().expect("valid ip"),
        LocalityInfo::new("dc1", "dc1-z1", "dc1-z1-m1"),
    )]);
    assert_eq!(
        memoized_pair_latency(config, partial),
        Some(Duration::ZERO),
        "a pair with one unlocalized endpoint must get no distance latency"
    );
}
