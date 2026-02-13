//! Simulation tests for actor state persistence and lifecycle.
//!
//! Tests the activate → mutate → persist → deactivate → reactivate → verify
//! cycle using `DeactivateOnIdle` actors with `PersistentState<T>`.
//!
//! # What's Tested
//!
//! - `on_activate` loads state from `InMemoryStateStore`
//! - `dispatch` mutates state and writes it to the store
//! - `DeactivateOnIdle` triggers deactivation after each dispatch
//! - Re-activation loads persisted state from the store
//! - State survives across deactivation/reactivation cycles

use std::rc::Rc;
use std::time::Duration;

use moonpool::actors::{
    ActorContext, ActorDirectory, ActorError, ActorHandler, ActorHost, ActorId, ActorRouter,
    ActorStateStore, ActorType, DeactivationHint, InMemoryDirectory, InMemoryStateStore,
    PersistentState, PlacementStrategy, RoundRobinPlacement,
};
use moonpool::{
    Endpoint, JsonCodec, MessageCodec, NetTransportBuilder, NetworkAddress, NetworkProvider,
    Providers, SimulationMetrics, SimulationResult, TaskProvider, TimeProvider,
    TokioStorageProvider, UID,
};
use moonpool_sim::{SimRandomProvider, SimulationBuilder, SimulationReport, WorkloadTopology};
use serde::{Deserialize, Serialize};

// ============================================================================
// WorkloadProviders (same pattern as actor_simulation.rs)
// ============================================================================

#[derive(Clone)]
struct WorkloadProviders<N, T, TP> {
    network: N,
    time: T,
    task: TP,
    random: SimRandomProvider,
    storage: TokioStorageProvider,
}

impl<N, T, TP> WorkloadProviders<N, T, TP>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    fn new(network: N, time: T, task: TP, random: SimRandomProvider) -> Self {
        Self {
            network,
            time,
            task,
            random,
            storage: TokioStorageProvider::new(),
        }
    }
}

impl<N, T, TP> Providers for WorkloadProviders<N, T, TP>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    type Network = N;
    type Time = T;
    type Task = TP;
    type Random = SimRandomProvider;
    type Storage = TokioStorageProvider;

    fn network(&self) -> &Self::Network {
        &self.network
    }
    fn time(&self) -> &Self::Time {
        &self.time
    }
    fn task(&self) -> &Self::Task {
        &self.task
    }
    fn random(&self) -> &Self::Random {
        &self.random
    }
    fn storage(&self) -> &Self::Storage {
        &self.storage
    }
}

// ============================================================================
// Stateful Counter Actor: uses PersistentState + DeactivateOnIdle
// ============================================================================

const STATEFUL_COUNTER_TYPE: ActorType = ActorType(0x57A7_EC70);

mod stateful_methods {
    pub const INCREMENT: u32 = 1;
    pub const GET_VALUE: u32 = 2;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IncrementRequest {
    amount: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GetValueRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ValueResponse {
    value: i64,
}

/// Persistent state for the counter — survives deactivation.
#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq)]
struct CounterData {
    value: i64,
}

/// A counter actor that persists state and deactivates after each message.
///
/// On every message:
/// 1. `on_activate` loads `PersistentState<CounterData>` from the store
/// 2. `dispatch` mutates and writes the state
/// 3. `deactivation_hint()` returns `DeactivateOnIdle`
/// 4. Host calls `on_deactivate` and removes the actor
///
/// Next message: step 1 again, loading persisted state from the store.
#[derive(Default)]
struct StatefulCounter {
    state: Option<PersistentState<CounterData>>,
}

#[async_trait::async_trait(?Send)]
impl ActorHandler for StatefulCounter {
    fn actor_type() -> ActorType {
        STATEFUL_COUNTER_TYPE
    }

    fn deactivation_hint(&self) -> DeactivationHint {
        DeactivationHint::DeactivateOnIdle
    }

    async fn on_activate<P: Providers, C: MessageCodec>(
        &mut self,
        ctx: &ActorContext<P, C>,
    ) -> Result<(), ActorError> {
        if let Some(store) = ctx.state_store() {
            let ps = PersistentState::<CounterData>::load(
                store.clone(),
                "StatefulCounter",
                &ctx.id.identity,
            )
            .await
            .map_err(|e| ActorError::HandlerError(format!("state load failed: {e}")))?;
            self.state = Some(ps);
        }
        Ok(())
    }

    async fn dispatch<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        method: u32,
        body: &[u8],
    ) -> Result<Vec<u8>, ActorError> {
        let codec = JsonCodec;

        // Get state — if no store was configured, just use in-memory default
        let current_value = self.state.as_ref().map(|s| s.state().value).unwrap_or(0);

        match method {
            stateful_methods::INCREMENT => {
                let req: IncrementRequest = codec.decode(body)?;
                let new_value = current_value + req.amount;

                if let Some(ps) = &mut self.state {
                    ps.state_mut().value = new_value;
                    ps.write_state().await.map_err(|e| {
                        ActorError::HandlerError(format!("state write failed: {e}"))
                    })?;
                }

                Ok(codec.encode(&ValueResponse { value: new_value })?)
            }
            stateful_methods::GET_VALUE => Ok(codec.encode(&ValueResponse {
                value: current_value,
            })?),
            _ => Err(ActorError::UnknownMethod(method)),
        }
    }
}

// ============================================================================
// Test Utilities
// ============================================================================

fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move { builder.run().await })
}

fn assert_simulation_success(report: &SimulationReport) {
    if !report.seeds_failing.is_empty() {
        panic!(
            "Simulation had {} failing seeds: {:?}",
            report.seeds_failing.len(),
            report.seeds_failing
        );
    }
}

// ============================================================================
// Workloads
// ============================================================================

/// Actor host workload with state store — runs on each simulated node.
#[allow(clippy::too_many_arguments)]
async fn stateful_host_workload<N, T, TP>(
    random: SimRandomProvider,
    network: N,
    time: T,
    task: TP,
    topology: WorkloadTopology,
    directory: Rc<dyn ActorDirectory>,
    placement: Rc<dyn PlacementStrategy>,
    state_store: Rc<dyn ActorStateStore>,
) -> SimulationResult<SimulationMetrics>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    let addr_with_port = if topology.my_ip.contains(':') {
        topology.my_ip.clone()
    } else {
        format!("{}:4500", topology.my_ip)
    };
    let local_addr = NetworkAddress::parse(&addr_with_port).expect("valid address");

    let providers = WorkloadProviders::new(network, time.clone(), task, random);
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await
        .expect("server listen failed");

    let router = Rc::new(ActorRouter::new(
        transport.clone(),
        directory.clone(),
        placement,
        JsonCodec,
    ));

    let host = ActorHost::new(transport, router, directory).with_state_store(state_store);
    host.register::<StatefulCounter>();

    loop {
        let _ = time.sleep(Duration::from_millis(100)).await;
        if topology.shutdown_signal.is_cancelled() {
            break;
        }
    }

    // Drop triggers close handles; the sim orchestrator processes remaining
    // events (including deactivation) via run_until_empty after all workloads exit.
    drop(host);

    Ok(SimulationMetrics {
        events_processed: 0,
        ..Default::default()
    })
}

/// Client workload — tests state persistence across deactivation/reactivation.
async fn stateful_client_workload<N, T, TP>(
    random: SimRandomProvider,
    network: N,
    time: T,
    task: TP,
    topology: WorkloadTopology,
    directory: Rc<dyn ActorDirectory>,
    placement: Rc<dyn PlacementStrategy>,
) -> SimulationResult<SimulationMetrics>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    let addr_with_port = if topology.my_ip.contains(':') {
        topology.my_ip.clone()
    } else {
        format!("{}:4500", topology.my_ip)
    };
    let local_addr = NetworkAddress::parse(&addr_with_port).expect("valid client address");

    let providers = WorkloadProviders::new(network, time.clone(), task, random);
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await
        .expect("client build failed");

    let router = Rc::new(ActorRouter::new(transport, directory, placement, JsonCodec));

    // Small delay to let server hosts start
    let _ = time.sleep(Duration::from_millis(50)).await;

    // --- Test 1: Increment alice ---
    // Actor activates, loads empty state, increments, writes, deactivates.
    let alice = ActorId::new(STATEFUL_COUNTER_TYPE, "alice");
    let resp: ValueResponse = router
        .send_actor_request(
            &alice,
            stateful_methods::INCREMENT,
            &IncrementRequest { amount: 100 },
        )
        .await
        .expect("alice increment should succeed");
    assert_eq!(
        resp.value, 100,
        "alice should have 100 after first increment"
    );

    // --- Test 2: Get alice value ---
    // Actor re-activates from store, loads persisted state (100).
    let resp: ValueResponse = router
        .send_actor_request(&alice, stateful_methods::GET_VALUE, &GetValueRequest {})
        .await
        .expect("alice get_value should succeed");
    assert_eq!(
        resp.value, 100,
        "alice should still have 100 after reactivation"
    );

    // --- Test 3: Increment alice again ---
    // Another reactivation, state should be 100, increment to 130.
    let resp: ValueResponse = router
        .send_actor_request(
            &alice,
            stateful_methods::INCREMENT,
            &IncrementRequest { amount: 30 },
        )
        .await
        .expect("alice second increment should succeed");
    assert_eq!(
        resp.value, 130,
        "alice should have 130 after second increment"
    );

    // --- Test 4: Independent actor bob ---
    let bob = ActorId::new(STATEFUL_COUNTER_TYPE, "bob");
    let resp: ValueResponse = router
        .send_actor_request(
            &bob,
            stateful_methods::INCREMENT,
            &IncrementRequest { amount: 50 },
        )
        .await
        .expect("bob increment should succeed");
    assert_eq!(resp.value, 50, "bob should have 50");

    // --- Test 5: Verify alice unchanged after bob's activity ---
    let resp: ValueResponse = router
        .send_actor_request(&alice, stateful_methods::GET_VALUE, &GetValueRequest {})
        .await
        .expect("alice final get should succeed");
    assert_eq!(resp.value, 130, "alice should still have 130");

    Ok(SimulationMetrics {
        events_processed: 5,
        ..Default::default()
    })
}

// ============================================================================
// Simulation Tests
// ============================================================================

/// Test state persistence across deactivation/reactivation with 3 server nodes.
///
/// Each server node has `DeactivateOnIdle` actors that persist state to a
/// shared `InMemoryStateStore`. After each message, the actor is deactivated.
/// The next message re-activates it and loads state from the store.
#[test]
fn test_stateful_actors_3x1() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let node_addrs = [
        NetworkAddress::parse("10.0.0.1:4500").expect("valid"),
        NetworkAddress::parse("10.0.0.2:4500").expect("valid"),
        NetworkAddress::parse("10.0.0.3:4500").expect("valid"),
    ];

    let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
    let state_store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());
    let node_endpoints: Vec<Endpoint> = node_addrs
        .iter()
        .map(|addr| Endpoint::new(addr.clone(), UID::new(STATEFUL_COUNTER_TYPE.0, 0)))
        .collect();
    let placement: Rc<dyn PlacementStrategy> = Rc::new(RoundRobinPlacement::new(node_endpoints));

    let report = run_simulation(
        SimulationBuilder::new()
            .register_workload("node1", {
                let dir = directory.clone();
                let plc = placement.clone();
                let store = state_store.clone();
                move |random, network, time, task, topology| {
                    stateful_host_workload(
                        random,
                        network,
                        time,
                        task,
                        topology,
                        dir.clone(),
                        plc.clone(),
                        store.clone(),
                    )
                }
            })
            .register_workload("node2", {
                let dir = directory.clone();
                let plc = placement.clone();
                let store = state_store.clone();
                move |random, network, time, task, topology| {
                    stateful_host_workload(
                        random,
                        network,
                        time,
                        task,
                        topology,
                        dir.clone(),
                        plc.clone(),
                        store.clone(),
                    )
                }
            })
            .register_workload("node3", {
                let dir = directory.clone();
                let plc = placement.clone();
                let store = state_store.clone();
                move |random, network, time, task, topology| {
                    stateful_host_workload(
                        random,
                        network,
                        time,
                        task,
                        topology,
                        dir.clone(),
                        plc.clone(),
                        store.clone(),
                    )
                }
            })
            .register_workload("client", {
                let dir = directory.clone();
                let plc = placement.clone();
                move |random, network, time, task, topology| {
                    stateful_client_workload(
                        random,
                        network,
                        time,
                        task,
                        topology,
                        dir.clone(),
                        plc.clone(),
                    )
                }
            })
            .set_iterations(1),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test state persistence with 2 nodes (minimal multi-node setup).
#[test]
fn test_stateful_actors_2x1() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let node_addrs = [
        NetworkAddress::parse("10.0.0.1:4500").expect("valid"),
        NetworkAddress::parse("10.0.0.2:4500").expect("valid"),
    ];

    let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
    let state_store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());
    let node_endpoints: Vec<Endpoint> = node_addrs
        .iter()
        .map(|addr| Endpoint::new(addr.clone(), UID::new(STATEFUL_COUNTER_TYPE.0, 0)))
        .collect();
    let placement: Rc<dyn PlacementStrategy> = Rc::new(RoundRobinPlacement::new(node_endpoints));

    let report = run_simulation(
        SimulationBuilder::new()
            .register_workload("node1", {
                let dir = directory.clone();
                let plc = placement.clone();
                let store = state_store.clone();
                move |random, network, time, task, topology| {
                    stateful_host_workload(
                        random,
                        network,
                        time,
                        task,
                        topology,
                        dir.clone(),
                        plc.clone(),
                        store.clone(),
                    )
                }
            })
            .register_workload("node2", {
                let dir = directory.clone();
                let plc = placement.clone();
                let store = state_store.clone();
                move |random, network, time, task, topology| {
                    stateful_host_workload(
                        random,
                        network,
                        time,
                        task,
                        topology,
                        dir.clone(),
                        plc.clone(),
                        store.clone(),
                    )
                }
            })
            .register_workload("client", {
                let dir = directory.clone();
                let plc = placement.clone();
                move |random, network, time, task, topology| {
                    stateful_client_workload(
                        random,
                        network,
                        time,
                        task,
                        topology,
                        dir.clone(),
                        plc.clone(),
                    )
                }
            })
            .set_iterations(1),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}
