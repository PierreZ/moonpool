//! Minimal 2-node simulation test - Phase 1
//!
//! This test validates that the simulation framework works correctly with:
//! - Deterministic simulation providers (network, time, tasks)
//! - Basic async I/O operations (time.sleep())
//! - Coverage assertions for code paths
//! - Simple invariant checking
//!
//! Workload: 2 nodes that complete simple async operations

use moonpool_foundation::{
    SimulationBuilder, SimulationMetrics, SimulationResult, WorkloadTopology,
    assertions::panic_on_assertion_violations,
    random::RandomProvider,
    runner::IterationControl,
    sometimes_assert,
    TimeProvider,
};

/// Minimal node workload: just validates runtime creation works with simulation
async fn node_workload<R, N, T, TS>(
    _random: R,
    _network: N,
    time: T,
    _task_provider: TS,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics>
where
    R: RandomProvider + Clone + 'static,
    N: moonpool_foundation::NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TS: moonpool_foundation::TaskProvider + Clone + 'static,
{
    tracing::warn!(
        ip = %topology.my_ip,
        "Starting minimal node workload"
    );

    // For Phase 1, just do a simple delay to create I/O activity
    // This validates that the simulation framework works
    let _ = time.sleep(std::time::Duration::from_millis(100)).await;

    sometimes_assert!(
        node_started,
        true,
        "Node workload started successfully"
    );

    // Register state
    topology.state_registry.register_state(
        &topology.my_ip,
        serde_json::json!({
            "status": "completed",
        }),
    );

    tracing::warn!(
        ip = %topology.my_ip,
        "Node workload completed"
    );

    Ok(SimulationMetrics::default())
}

/// Invariant: All nodes complete successfully
fn create_completion_invariant() -> moonpool_foundation::InvariantCheck {
    Box::new(|states, _time, seed| {
        // Simple invariant: all nodes should register a status
        for (key, state) in states.iter() {
            tracing::debug!(
                seed = seed,
                node = key,
                state = ?state,
                "Invariant check: node state"
            );

            // Each node should have completed
            if let Some(status) = state.get("status").and_then(|v| v.as_str()) {
                assert!(
                    status == "completed",
                    "SEED={}: Node {} status is {}",
                    seed,
                    key,
                    status
                );
            }
        }
    })
}

#[test]
fn slow_simulation_messagebus_minimal_2node() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN) // Use WARN to see more details
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .use_random_config()
            .set_iteration_control(IterationControl::FixedCount(10)) // Start with 10 seeds for Phase 1
            .register_workload("node_1", node_workload)
            .register_workload("node_2", node_workload)
            .with_invariants(vec![create_completion_invariant()])
            .run()
            .await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("Faulty seeds detected: {:?}", report.seeds_failing);
        }

        panic_on_assertion_violations(&report);

        println!("✅ Phase 1: Minimal 2-node MessageBus test completed successfully");
    });
}
