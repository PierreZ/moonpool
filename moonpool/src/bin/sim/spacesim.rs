//! Binary target for space economy simulation.

use std::rc::Rc;

use moonpool::actors::{
    ActorDirectory, ActorStateStore, ClusterConfig, InMemoryDirectory, InMemoryStateStore,
    SharedMembership,
};
use moonpool::simulations::spacesim::{
    invariants::{CreditConservation, NonNegativeBalances},
    workloads::{SpaceProcess, SpaceWorkload},
};
use moonpool_sim::{AdaptiveConfig, ExplorationConfig, Parallelism, SimulationBuilder};

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    let membership = Rc::new(SharedMembership::new());
    let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
    let state_store = Rc::new(InMemoryStateStore::new());

    let cluster = ClusterConfig::builder()
        .name("spacesim")
        .membership(membership.clone())
        .directory(directory.clone())
        .build()
        .expect("cluster config");

    let stations = ["alpha", "beta", "gamma", "delta", "epsilon"];

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    let report = local_runtime.block_on(async move {
        SimulationBuilder::new()
            .processes(1, {
                let cluster = cluster.clone();
                let ss = state_store.clone() as Rc<dyn ActorStateStore>;
                move || {
                    Box::new(SpaceProcess::new(cluster.clone(), ss.clone()))
                        as Box<dyn moonpool_sim::Process>
                }
            })
            .workload(SpaceWorkload::new(200, &stations, cluster, state_store))
            .invariant(CreditConservation)
            .invariant(NonNegativeBalances)
            .enable_exploration(ExplorationConfig {
                max_depth: 30,
                timelines_per_split: 4,
                global_energy: 20_000,
                adaptive: Some(AdaptiveConfig {
                    batch_size: 20,
                    min_timelines: 60,
                    max_timelines: 200,
                    per_mark_energy: 1_000,
                    warm_min_timelines: Some(20),
                }),
                parallelism: Some(Parallelism::Cores(3)),
            })
            .until_converged(10)
            .run()
            .await
    });

    report.eprint();

    if !report.is_success() {
        std::process::exit(1);
    }
}
