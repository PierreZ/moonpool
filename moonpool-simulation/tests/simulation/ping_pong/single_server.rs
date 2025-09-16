use moonpool_simulation::{
    NetworkRandomizationRanges, SimulationBuilder, SimulationMetrics, SimulationResult,
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
            .set_randomization_ranges(NetworkRandomizationRanges::stable_testing())
            .register_workload("ping_pong_server", ping_pong_server)
            .register_workload("ping_pong_client", ping_pong_client)
            .set_iteration_control(IterationControl::UntilAllSometimesReached(10_000))
            //.set_debug_seeds(vec![15302657452152344853])
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
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> SimulationResult<SimulationMetrics> {
    tracing::debug!("SERVER WORKLOAD: Starting");
    let mut server_actor = PingPongServerActor::new(
        provider,
        time_provider,
        task_provider,
        topology,
        shutdown_rx,
    );
    let result = server_actor.run().await;
    tracing::debug!("SERVER WORKLOAD: Exiting with result: {:?}", result.is_ok());
    result
}

/// Client workload for ping-pong communication
async fn ping_pong_client(
    _seed: u64,
    provider: moonpool_simulation::SimNetworkProvider,
    time_provider: moonpool_simulation::SimTimeProvider,
    task_provider: moonpool_simulation::TokioTaskProvider,
    topology: moonpool_simulation::WorkloadTopology,
    _shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> SimulationResult<SimulationMetrics> {
    tracing::debug!("CLIENT WORKLOAD: Starting");
    let mut client_actor =
        PingPongClientActor::new(provider, time_provider, task_provider, topology);
    let result = client_actor.run().await;
    tracing::debug!("CLIENT WORKLOAD: Exiting with result: {:?}", result.is_ok());
    result
}
