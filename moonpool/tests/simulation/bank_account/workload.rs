//! Simulation workloads for BankAccountActor testing.
//!
//! This module defines workloads for testing location-transparent actor
//! messaging in various topologies (1x1, 2x2, 10x10).

use moonpool_foundation::{
    SimNetworkProvider, SimRandomProvider, SimTimeProvider, SimulationMetrics, SimulationResult,
    TokioTaskProvider, WorkloadTopology,
};

#[allow(unused_imports)]
use super::actor::{
    BankAccountActor, DepositRequest, GetBalanceRequest, WithdrawRequest,
    dispatch_bank_account_message,
};
use moonpool::prelude::*;
use std::rc::Rc;

/// Single-node workload (1x1 topology).
///
/// **Purpose**: Basic functionality validation.
/// - 1 actor runtime
/// - 1 BankAccountActor
/// - Sequential operations (deposit, withdraw, check balance)
///
/// **Success Criteria**:
/// - Actor activates successfully
/// - All messages processed in order
/// - Final balance matches expected value
///
/// TODO: Refactor to work with automatic message processing.
/// This workload uses `process_message_queue()` which no longer exists.
/// Messages are now processed automatically by the message loop task.
/// Need to update to use async message passing without manual processing.
#[allow(dead_code)]
pub async fn single_node_workload(
    _random: SimRandomProvider,
    _network: SimNetworkProvider,
    _time: SimTimeProvider,
    _task_provider: TokioTaskProvider,
    _topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    tracing::info!("Starting single_node_workload (1x1)");

    // Step 1: Create infrastructure manually (ActorRuntime builder not ready yet)
    let node_id = NodeId::from("127.0.0.1:5000").expect("Failed to create NodeId");
    let message_bus = Rc::new(moonpool::messaging::MessageBus::new(node_id.clone()));

    // Step 2: Create actor manually and get context
    let actor_id =
        ActorId::from_string("test::BankAccount/alice").expect("Failed to create ActorId");
    let actor = BankAccountActor::new(actor_id.clone());

    // Get catalog for BankAccountActor
    // Note: In Phase 3, we need to manually access the catalog
    // In Phase 4, this will be abstracted away
    let catalog = Rc::new(ActorCatalog::<BankAccountActor, _>::new(node_id, _task_provider));

    // Set MessageBus on catalog
    catalog.set_message_bus(message_bus.clone());

    // Set catalog as the actor router on the message bus
    message_bus.set_actor_router(catalog.clone());

    // Create activation
    let context = catalog
        .get_or_create_activation(actor_id.clone(), actor)
        .expect("Failed to create activation");

    // Activate the actor
    context
        .activate(None)
        .await
        .expect("Failed to activate actor");

    // Set MessageBus on context
    context.set_message_bus(message_bus.clone());

    // Step 3: Get ActorRef with MessageBus
    let actor_ref =
        ActorRef::<BankAccountActor>::with_message_bus(actor_id.clone(), message_bus.clone());

    // Step 4: Perform operations
    // TODO: The old code used process_message_queue() which no longer exists.
    // Messages are now automatically processed by the message loop task.
    // This test needs refactoring to work with the new async architecture.

    tracing::info!("single_node_workload - SKIPPED (needs refactoring for automatic message processing)");

    // Suppress unused variable warnings
    let _ = (context, actor_ref);

    Ok(SimulationMetrics::default())
}

/// Multi-node workload (2x2 topology).
///
/// **Purpose**: Distributed scenario validation.
/// - 2 actor runtimes (node A, node B)
/// - 2 BankAccountActors (alice, bob)
/// - Cross-node messaging (node A calls actor on node B)
///
/// **Success Criteria**:
/// - Actors activate on different nodes
/// - Messages route correctly across nodes
/// - Directory tracks actor locations
/// - All operations complete successfully
pub async fn multi_node_workload(
    _random: SimRandomProvider,
    _network: SimNetworkProvider,
    _time: SimTimeProvider,
    _task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    tracing::info!(
        "Starting multi_node_workload (2x2), topology: {:?}",
        topology
    );

    // TODO: Create 2 ActorRuntimes (node1, node2)
    // TODO: Share Directory between nodes
    // TODO: Get ActorRef for alice (from node1)
    // TODO: Get ActorRef for bob (from node2)
    // TODO: Perform operations:
    //   - node1: alice.deposit(100)
    //   - node2: bob.deposit(200)
    //   - node1: alice.withdraw(30)
    //   - node2: bob.get_balance()
    // TODO: Verify both actors processed correctly

    tracing::info!("multi_node_workload completed");

    Ok(SimulationMetrics::default())
}

/// Concurrent deposit workload (for User Story 2).
///
/// **Purpose**: Test sequential message processing under concurrency.
/// - 100 concurrent deposits to same actor
/// - Verify balance invariant (sum of deposits)
///
/// **Success Criteria** (Phase 4 - User Story 2):
/// - No race conditions
/// - Messages processed sequentially per actor
/// - Final balance = sum of all deposits
pub async fn concurrent_deposit_workload(
    _random: SimRandomProvider,
    _network: SimNetworkProvider,
    _time: SimTimeProvider,
    _task_provider: TokioTaskProvider,
    _topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    tracing::info!("Starting concurrent_deposit_workload");

    // TODO (Phase 4): Implement concurrent deposit testing
    // - Spawn 100 tasks that deposit to same actor
    // - Verify final balance = 100 * deposit_amount
    // - Check no messages lost

    tracing::info!("concurrent_deposit_workload completed");

    Ok(SimulationMetrics::default())
}

/// Multi-actor workload (10x10 topology - for User Story 3).
///
/// **Purpose**: Test placement algorithm and directory load balancing.
/// - 10 nodes
/// - 100 actors distributed across nodes
/// - Verify even distribution (within 20% variance)
///
/// **Success Criteria** (Phase 5 - User Story 3):
/// - Actors distributed evenly (two-random-choices)
/// - Directory tracks all locations correctly
/// - Cross-node messages route correctly
/// - Placement variance ≤ 20%
pub async fn multi_actor_workload(
    _random: SimRandomProvider,
    _network: SimNetworkProvider,
    _time: SimTimeProvider,
    _task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    tracing::info!(
        "Starting multi_actor_workload (10x10), topology: {:?}",
        topology
    );

    // TODO (Phase 5): Implement multi-actor placement testing
    // - Create 10 ActorRuntimes
    // - Activate 100 actors
    // - Measure distribution across nodes
    // - Verify variance within 20%

    tracing::info!("multi_actor_workload completed");

    Ok(SimulationMetrics::default())
}

/// Timeout test workload (for User Story 4).
///
/// **Purpose**: Test request-response timeout enforcement.
/// - Send requests with short timeouts to slow actors
/// - Verify timeout errors returned correctly
///
/// **Success Criteria** (Phase 6 - User Story 4):
/// - Timeouts trigger within 10% of configured duration
/// - Correlation IDs cleaned up after timeout
/// - No late responses delivered
pub async fn timeout_test_workload(
    _random: SimRandomProvider,
    _network: SimNetworkProvider,
    _time: SimTimeProvider,
    _task_provider: TokioTaskProvider,
    _topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    tracing::info!("Starting timeout_test_workload");

    // TODO (Phase 6): Implement timeout testing
    // - Create slow actor (with delays)
    // - Send request with short timeout
    // - Verify ActorError::Timeout returned

    tracing::info!("timeout_test_workload completed");

    Ok(SimulationMetrics::default())
}

/// Persistence test workload (for User Story 5).
///
/// **Purpose**: Test state persistence across deactivation/reactivation.
/// - Deposit money, deactivate, reactivate, verify balance
///
/// **Success Criteria** (Phase 7 - User Story 5):
/// - State persisted correctly
/// - on_activate() receives previously saved state
/// - Balance survives deactivation
pub async fn persistence_test_workload(
    _random: SimRandomProvider,
    _network: SimNetworkProvider,
    _time: SimTimeProvider,
    _task_provider: TokioTaskProvider,
    _topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    tracing::info!("Starting persistence_test_workload");

    // TODO (Phase 7): Implement persistence testing
    // - Create runtime with InMemoryStorage
    // - actor.deposit(500)
    // - runtime.deactivate_actor(actor_id)
    // - actor.get_balance() (triggers reactivation)
    // - Verify balance == 500

    tracing::info!("persistence_test_workload completed");

    Ok(SimulationMetrics::default())
}
