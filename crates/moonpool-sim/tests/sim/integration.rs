use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll};

use futures::{io::AsyncWriteExt, task::noop_waker};
use moonpool_sim::{NetworkConfiguration, NetworkProvider, SimWorld, TcpListenerTrait};

const MAX_DRIVER_STEPS: usize = 100_000;

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
    panic!("simulation-backed future exceeded {MAX_DRIVER_STEPS} events");
}

#[test]
fn test_basic_simulation_bind() {
    let mut sim = SimWorld::new();
    let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

    let listener = drive(&mut sim, provider.bind("test-addr")).expect("listener binds");
    assert_eq!(
        listener.local_addr().expect("listener has an address"),
        "test-addr"
    );
}

// Simple echo server that reads once and writes back
async fn simple_echo_server<P>(provider: P, addr: &str) -> std::io::Result<()>
where
    P: NetworkProvider + Clone,
{
    let listener = provider.bind(addr).await?;
    let _client = provider.connect(addr).await?; // Create connection first
    let (mut stream, _peer_addr) = listener.accept().await?; // Then accept it

    // Write some test data to exercise the connection
    let test_data = b"Hello from simple echo server";
    stream.write_all(test_data).await?;

    Ok(())
}

#[test]
fn test_simple_echo_simulation() {
    let config = NetworkConfiguration::fast_local();
    let mut sim = SimWorld::new_with_network_config(config);
    let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

    drive(&mut sim, simple_echo_server(provider, "echo-server")).expect("echo exchange succeeds");
    sim.run_until_empty();

    assert!(sim.current_time() > std::time::Duration::ZERO);
}

#[test]
fn test_deterministic_simulation_behavior() {
    let execution_times = (0..3)
        .map(|_| {
            let mut sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());
            let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));
            drive(&mut sim, simple_echo_server(provider, "deterministic-test"))
                .expect("echo exchange succeeds");
            sim.run_until_empty();
            sim.current_time()
        })
        .collect::<Vec<_>>();

    assert!(
        execution_times.windows(2).all(|times| times[0] == times[1]),
        "identical configurations must produce identical timings: {execution_times:?}"
    );
}

/// Test that `SimNetworkProvider` can be used generically.
async fn use_provider_generically<P: NetworkProvider>(
    provider: P,
    addr: &str,
) -> std::io::Result<String> {
    let listener = provider.bind(addr).await?;
    listener.local_addr()
}

#[test]
fn test_network_provider_trait_usage() {
    let mut sim = SimWorld::new();
    let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));

    let addr = drive(&mut sim, use_provider_generically(provider, "dynamic-test"))
        .expect("generic provider binds");
    assert_eq!(addr, "dynamic-test");
}
