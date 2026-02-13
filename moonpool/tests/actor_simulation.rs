//! Simulation tests for multi-node virtual actors.
//!
//! Tests ActorHost, ActorRouter, and forwarding under simulated multi-node
//! topologies using the moonpool-sim framework.
//!
//! # What's Tested
//!
//! - Multiple simulated nodes, each with their own transport + ActorHost
//! - Shared `InMemoryDirectory` (per design: no coordinator, all nodes
//!   share the directory)
//! - `RoundRobinPlacement` distributes actors across nodes
//! - Cross-node actor calls: caller on node A, actor on node B
//! - Multiple actors (alice, bob, charlie) living on different nodes
//! - State persistence across calls

use std::rc::Rc;
use std::time::Duration;

use moonpool::actors::{
    ActorContext, ActorDirectory, ActorError, ActorHandler, ActorHost, ActorId, ActorRouter,
    ActorType, InMemoryDirectory, PlacementStrategy, RoundRobinPlacement,
};
use moonpool::{
    Endpoint, JsonCodec, MessageCodec, NetTransportBuilder, NetworkAddress, NetworkProvider,
    Providers, SimulationMetrics, SimulationResult, TaskProvider, TimeProvider,
    TokioStorageProvider, UID,
};
use moonpool_sim::{SimRandomProvider, SimulationBuilder, SimulationReport, WorkloadTopology};
use serde::{Deserialize, Serialize};

// ============================================================================
// WorkloadProviders (same pattern as transport simulation tests)
// ============================================================================

/// Providers bundle for simulation workloads.
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
// Test Actor: Counter (simple state for verification)
// ============================================================================

const COUNTER_ACTOR_TYPE: ActorType = ActorType(0x7E57_AC70);

mod counter_methods {
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

/// Simple counter actor for testing multi-node dispatch.
#[derive(Default)]
struct CounterActor {
    value: i64,
}

#[async_trait::async_trait(?Send)]
impl ActorHandler for CounterActor {
    fn actor_type() -> ActorType {
        COUNTER_ACTOR_TYPE
    }

    async fn dispatch<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        method: u32,
        body: &[u8],
    ) -> Result<Vec<u8>, ActorError> {
        let codec = JsonCodec;
        match method {
            counter_methods::INCREMENT => {
                let req: IncrementRequest = codec.decode(body)?;
                self.value += req.amount;
                Ok(codec.encode(&ValueResponse { value: self.value })?)
            }
            counter_methods::GET_VALUE => Ok(codec.encode(&ValueResponse { value: self.value })?),
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

/// Actor host workload — runs on each simulated node.
///
/// Each node:
/// 1. Creates a listening transport
/// 2. Creates an ActorRouter and ActorHost
/// 3. Registers CounterActor
/// 4. Stays alive via time.sleep() loop until shutdown signal
///
/// Uses `time.sleep()` to generate simulation events and avoid deadlock.
async fn actor_host_workload<N, T, TP>(
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

    let host = ActorHost::new(transport, router, directory);
    host.register::<CounterActor>();

    // Keep alive by sleeping in a loop, generating simulation events.
    // The shutdown signal fires when the client workload returns Ok.
    loop {
        let _ = time.sleep(Duration::from_millis(100)).await;
        if topology.shutdown_signal.is_cancelled() {
            break;
        }
    }

    Ok(SimulationMetrics {
        events_processed: 0,
        ..Default::default()
    })
}

/// Client workload — drives actor calls across nodes.
///
/// The client:
/// 1. Creates a listening transport (needed for responses in simulation —
///    the server sends responses by opening a new outgoing connection to
///    the client's address, because simulated `accept()` uses ephemeral
///    addresses so the server can't reuse the incoming connection)
/// 2. Builds a router pointing at the shared directory + placement
/// 3. Sends actor requests that route to different nodes
/// 4. Verifies state is maintained correctly per actor identity
async fn actor_client_workload<N, T, TP>(
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

    // Small delay to let server hosts start and begin listening.
    // time.sleep() generates a simulation event that advances simulated time.
    let _ = time.sleep(Duration::from_millis(50)).await;

    // --- Test 1: Basic cross-node deposit ---
    let alice = ActorId::new(COUNTER_ACTOR_TYPE, "alice");
    let resp: ValueResponse = router
        .send_actor_request(
            &alice,
            counter_methods::INCREMENT,
            &IncrementRequest { amount: 100 },
        )
        .await
        .expect("alice deposit should succeed");
    assert_eq!(resp.value, 100, "alice should have 100 after first deposit");

    // --- Test 2: Different actor on potentially different node ---
    let bob = ActorId::new(COUNTER_ACTOR_TYPE, "bob");
    let resp: ValueResponse = router
        .send_actor_request(
            &bob,
            counter_methods::INCREMENT,
            &IncrementRequest { amount: 50 },
        )
        .await
        .expect("bob deposit should succeed");
    assert_eq!(resp.value, 50, "bob should have 50 after deposit");

    // --- Test 3: State persistence - second call to alice ---
    let resp: ValueResponse = router
        .send_actor_request(
            &alice,
            counter_methods::INCREMENT,
            &IncrementRequest { amount: 30 },
        )
        .await
        .expect("alice second deposit should succeed");
    assert_eq!(
        resp.value, 130,
        "alice should have 130 after second deposit"
    );

    // --- Test 4: Get balance from alice ---
    let resp: ValueResponse = router
        .send_actor_request(&alice, counter_methods::GET_VALUE, &GetValueRequest {})
        .await
        .expect("alice get_value should succeed");
    assert_eq!(resp.value, 130, "alice final balance should be 130");

    // --- Test 5: Get balance from bob ---
    let resp: ValueResponse = router
        .send_actor_request(&bob, counter_methods::GET_VALUE, &GetValueRequest {})
        .await
        .expect("bob get_value should succeed");
    assert_eq!(resp.value, 50, "bob final balance should be 50");

    // --- Test 6: Third actor to exercise round-robin placement ---
    let charlie = ActorId::new(COUNTER_ACTOR_TYPE, "charlie");
    let resp: ValueResponse = router
        .send_actor_request(
            &charlie,
            counter_methods::INCREMENT,
            &IncrementRequest { amount: 77 },
        )
        .await
        .expect("charlie deposit should succeed");
    assert_eq!(resp.value, 77, "charlie should have 77 after deposit");

    Ok(SimulationMetrics {
        events_processed: 6,
        ..Default::default()
    })
}

// ============================================================================
// Simulation Tests
// ============================================================================

/// Test multi-node actor dispatch with 3 server nodes and 1 client.
///
/// Each server node runs ActorHost in its own simulated network.
/// RoundRobinPlacement distributes actors across the 3 nodes.
/// Shared InMemoryDirectory means all nodes see each other's registrations.
#[test]
fn test_multi_node_actors_3x1() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    // Pre-build shared state — directory and placement shared across all
    // workloads within the same iteration.
    let node_addrs = [
        NetworkAddress::parse("10.0.0.1:4500").expect("valid"),
        NetworkAddress::parse("10.0.0.2:4500").expect("valid"),
        NetworkAddress::parse("10.0.0.3:4500").expect("valid"),
    ];

    let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
    let node_endpoints: Vec<Endpoint> = node_addrs
        .iter()
        .map(|addr| Endpoint::new(addr.clone(), UID::new(COUNTER_ACTOR_TYPE.0, 0)))
        .collect();
    let placement: Rc<dyn PlacementStrategy> = Rc::new(RoundRobinPlacement::new(node_endpoints));

    let report = run_simulation(
        SimulationBuilder::new()
            // Node 1 → 10.0.0.1
            .register_workload("node1", {
                let dir = directory.clone();
                let plc = placement.clone();
                move |random, network, time, task, topology| {
                    actor_host_workload(
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
            // Node 2 → 10.0.0.2
            .register_workload("node2", {
                let dir = directory.clone();
                let plc = placement.clone();
                move |random, network, time, task, topology| {
                    actor_host_workload(
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
            // Node 3 → 10.0.0.3
            .register_workload("node3", {
                let dir = directory.clone();
                let plc = placement.clone();
                move |random, network, time, task, topology| {
                    actor_host_workload(
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
            // Client → 10.0.0.4
            .register_workload("client", {
                let dir = directory.clone();
                let plc = placement.clone();
                move |random, network, time, task, topology| {
                    actor_client_workload(
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

/// Test multi-node actors with 2 nodes (minimal multi-node setup).
#[test]
fn test_multi_node_actors_2x1() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let node_addrs = [
        NetworkAddress::parse("10.0.0.1:4500").expect("valid"),
        NetworkAddress::parse("10.0.0.2:4500").expect("valid"),
    ];

    let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
    let node_endpoints: Vec<Endpoint> = node_addrs
        .iter()
        .map(|addr| Endpoint::new(addr.clone(), UID::new(COUNTER_ACTOR_TYPE.0, 0)))
        .collect();
    let placement: Rc<dyn PlacementStrategy> = Rc::new(RoundRobinPlacement::new(node_endpoints));

    let report = run_simulation(
        SimulationBuilder::new()
            .register_workload("node1", {
                let dir = directory.clone();
                let plc = placement.clone();
                move |random, network, time, task, topology| {
                    actor_host_workload(
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
            .register_workload("node2", {
                let dir = directory.clone();
                let plc = placement.clone();
                move |random, network, time, task, topology| {
                    actor_host_workload(
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
            .register_workload("client", {
                let dir = directory.clone();
                let plc = placement.clone();
                move |random, network, time, task, topology| {
                    actor_client_workload(
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
