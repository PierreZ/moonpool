//! Binary target for space economy simulation.

use std::rc::Rc;

use moonpool::actors::{
    ActorDirectory, ActorStateStore, ClusterConfig, InMemoryDirectory, InMemoryStateStore,
    SharedMembership,
};
use moonpool::simulations::invariants::DirectoryConsistency;
use moonpool::simulations::spacesim::{
    invariants::{CargoConservation, CreditConservation, NonNegativeBalances},
    workloads::{SpaceProcess, SpaceWorkload},
};
use moonpool_sim::SimulationBuilder;
use tokio::runtime::RngSeed;

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    let membership = Rc::new(SharedMembership::new());
    let directory = Rc::new(InMemoryDirectory::new());
    let state_store = Rc::new(InMemoryStateStore::new());

    let cluster = ClusterConfig::builder()
        .name("spacesim")
        .membership(membership.clone() as Rc<dyn moonpool::actors::MembershipProvider>)
        .directory(directory.clone() as Rc<dyn ActorDirectory>)
        .build()
        .expect("cluster config");

    let stations = ["alpha", "beta", "gamma", "delta", "epsilon"];
    let seed = RngSeed::from_bytes(b"my_fixed_seed_001");

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .rng_seed(seed)
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    let report = local_runtime.block_on(async move {
        SimulationBuilder::new()
            .before_iteration({
                let m = membership.clone();
                let d = directory.clone();
                let s = state_store.clone();
                move || {
                    m.clear();
                    d.clear();
                    s.clear();
                }
            })
            .processes(1, {
                let cluster = cluster.clone();
                let ss = state_store.clone() as Rc<dyn ActorStateStore>;
                move || {
                    Box::new(SpaceProcess::new(cluster.clone(), ss.clone()))
                        as Box<dyn moonpool_sim::Process>
                }
            })
            .workload(SpaceWorkload::new(
                200, &stations, cluster, directory, membership,
            ))
            .invariant(CreditConservation)
            .invariant(CargoConservation)
            .invariant(NonNegativeBalances)
            .invariant(DirectoryConsistency)
            // .enable_exploration(ExplorationConfig {
            //     max_depth: 30,
            //     timelines_per_split: 4,
            //     global_energy: 20_000,
            //     adaptive: Some(AdaptiveConfig {
            //         batch_size: 20,
            //         min_timelines: 60,
            //         max_timelines: 200,
            //         per_mark_energy: 1_000,
            //         warm_min_timelines: Some(20),
            //     }),
            //     parallelism: None,
            // })
            // .until_converged(10)
            .set_debug_seeds(vec![
                3937336182698727299, //, 14597617647669048509, 6767011624512909954
            ])
            .set_iterations(1)
            .run()
            .await
    });

    report.eprint();

    if !report.is_success() {
        std::process::exit(1);
    }
}
