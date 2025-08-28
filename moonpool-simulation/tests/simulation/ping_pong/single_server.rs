use moonpool_simulation::{
    NetworkRandomizationRanges, SimulationBuilder, SimulationMetrics, SimulationResult,
    runner::IterationControl,
};
use tracing::Level;
use tracing_subscriber;

use super::actors::{PingPongClientActor, PingPongServerActor};

// TODO: Check sometimes assert

#[test]
fn test_ping_pong_with_simulation_builder() {
    let iteration_count = 50;
    let check_assert = true;
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(Level::WARN)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .set_randomization_ranges(NetworkRandomizationRanges::chaos_testing())
            .register_workload("ping_pong_server", ping_pong_server)
            .register_workload("ping_pong_client", ping_pong_client)
            .set_iteration_control(IterationControl::FixedCount(iteration_count))
            .set_debug_seeds(vec![42, 9495001370864752853, 123456, 999888777, 111222333]) // Test multiple seeds including the previously failing one
            .run()
            .await;

        // Display comprehensive simulation report with assertion validation
        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("faulty seeds detected: {:?}", report.seeds_failing);
        }

        // The new SimulationReport automatically includes assertion validation
        if report.assertion_validation.has_violations() {
            if !report
                .assertion_validation
                .success_rate_violations
                .is_empty()
            {
                println!("❌ Success rate violations found:");
                for violation in &report.assertion_validation.success_rate_violations {
                    println!("  - {}", violation);
                }
                if check_assert {
                    panic!("❌ Unexpected success rate violations detected!");
                }
            }

            if !report
                .assertion_validation
                .unreachable_assertions
                .is_empty()
            {
                println!("⚠️ Unreachable code detected:");
                for violation in &report.assertion_validation.unreachable_assertions {
                    println!("  - {}", violation);
                }
                if check_assert {
                    panic!("❌ Unexpected unreachable assertions detected!");
                }
            }
        } else {
            println!("");
            println!("✅ Dynamic validation passed - no assertion violations detected!");
        }
    });
}

/// Server workload for ping-pong communication
async fn ping_pong_server(
    _seed: u64,
    provider: moonpool_simulation::SimNetworkProvider,
    time_provider: moonpool_simulation::SimTimeProvider,
    topology: moonpool_simulation::WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let mut server_actor = PingPongServerActor::new(provider, time_provider, topology);
    server_actor.run().await
}

/// Client workload for ping-pong communication
async fn ping_pong_client(
    _seed: u64,
    provider: moonpool_simulation::SimNetworkProvider,
    time_provider: moonpool_simulation::SimTimeProvider,
    topology: moonpool_simulation::WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let mut client_actor = PingPongClientActor::new(provider, time_provider, topology);
    client_actor.run().await
}
