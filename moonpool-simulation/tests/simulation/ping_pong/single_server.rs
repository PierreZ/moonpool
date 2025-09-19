use moonpool_simulation::{
    SimulationBuilder, SimulationMetrics, SimulationResult, TokioNetworkProvider, TokioRunner,
    TokioTaskProvider, TokioTimeProvider, WorkloadTopology,
    assertions::panic_on_assertion_violations, runner::IterationControl,
};
use tracing::Level;
use tracing_subscriber;

use super::actors::{PingPongClientActor, PingPongServerActor};

#[test]
fn test_ping_pong_with_simulation_builder() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(Level::DEBUG)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .use_random_config()
            .register_workload("ping_pong_server", ping_pong_server)
            .register_workload("ping_pong_client", ping_pong_client)
            .set_iteration_control(IterationControl::FixedCount(1))
            .set_debug_seeds(vec![42])
            .run()
            .await;

        // Display comprehensive simulation report with assertion validation
        println!("{}", report);

        // With chaos testing, some seeds may fail due to network disruption - this is expected
        if !report.seeds_failing.is_empty() {
            panic!("faulty seeds detected: {:?}", report.seeds_failing);
        }

        // Validate assertion contracts using the helper function
        panic_on_assertion_violations(&report);
    });
}

/// Server workload for ping-pong communication
async fn ping_pong_server(
    _seed: u64,
    provider: moonpool_simulation::SimNetworkProvider,
    time_provider: moonpool_simulation::SimTimeProvider,
    task_provider: moonpool_simulation::TokioTaskProvider,
    topology: moonpool_simulation::WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let mut server_actor =
        PingPongServerActor::new(provider, time_provider, task_provider, topology);
    server_actor.run().await
}

/// Client workload for ping-pong communication
async fn ping_pong_client(
    _seed: u64,
    provider: moonpool_simulation::SimNetworkProvider,
    time_provider: moonpool_simulation::SimTimeProvider,
    task_provider: moonpool_simulation::TokioTaskProvider,
    topology: moonpool_simulation::WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let mut client_actor =
        PingPongClientActor::new(provider, time_provider, task_provider, topology);
    client_actor.run().await
}

#[test]
fn test_ping_pong_with_tokio_runner() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(Level::INFO)
        .try_init();

    // Create single-threaded Tokio runtime for deterministic execution
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    let report = local_runtime.block_on(async move {
        TokioRunner::new()
            .register_workload("ping_pong_server", tokio_ping_pong_server)
            .register_workload("ping_pong_client", tokio_ping_pong_client)
            .run()
            .await
    });

    // Display the report
    println!("{}", report);

    // Validate that both workloads completed successfully
    assert_eq!(
        report.successful, 2,
        "Both server and client should succeed"
    );
    assert_eq!(report.failed, 0, "No failures expected");
    assert_eq!(report.success_rate(), 100.0);

    println!("✅ TokioRunner ping-pong test completed successfully");
}

/// Adapter function to run server actor with TokioRunner signature
async fn tokio_ping_pong_server(
    provider: TokioNetworkProvider,
    time_provider: TokioTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let mut server_actor =
        PingPongServerActor::new(provider, time_provider, task_provider, topology);
    server_actor.run().await
}

/// Adapter function to run client actor with TokioRunner signature
async fn tokio_ping_pong_client(
    provider: TokioNetworkProvider,
    time_provider: TokioTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let mut client_actor =
        PingPongClientActor::new(provider, time_provider, task_provider, topology);
    client_actor.run().await
}
