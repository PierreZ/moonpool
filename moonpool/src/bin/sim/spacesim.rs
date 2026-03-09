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

/// Parse `--seeds 42,123,999` from command-line arguments.
fn parse_seeds() -> Option<Vec<u64>> {
    let args: Vec<String> = std::env::args().collect();
    let pos = args.iter().position(|a| a == "--seeds")?;
    let raw = args.get(pos + 1).unwrap_or_else(|| {
        eprintln!("error: --seeds requires a comma-separated list of u64 values");
        std::process::exit(1);
    });
    let seeds: Vec<u64> = raw
        .split(',')
        .map(|s| {
            s.trim().parse::<u64>().unwrap_or_else(|e| {
                eprintln!("error: invalid seed '{s}': {e}");
                std::process::exit(1);
            })
        })
        .collect();
    Some(seeds)
}

fn main() {
    let debug_seeds = parse_seeds();

    let log_level = if debug_seeds.is_some() {
        tracing::Level::DEBUG
    } else {
        tracing::Level::WARN
    };
    let _ = tracing_subscriber::fmt()
        .with_max_level(log_level)
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
    let ships = ["ship-1", "ship-2", "ship-3"];

    let builder = SimulationBuilder::new()
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
        .processes(3, {
            let cluster = cluster.clone();
            let ss = state_store.clone() as Rc<dyn ActorStateStore>;
            move || {
                Box::new(SpaceProcess::new(cluster.clone(), ss.clone()))
                    as Box<dyn moonpool_sim::Process>
            }
        })
        .workload(SpaceWorkload::new(
            200,
            &stations,
            &ships,
            cluster,
            directory,
            membership,
            state_store.clone() as Rc<dyn ActorStateStore>,
        ))
        .invariant(CreditConservation)
        .invariant(CargoConservation)
        .invariant(NonNegativeBalances)
        .invariant(DirectoryConsistency);

    let builder = if let Some(ref seeds) = debug_seeds {
        builder
            .set_debug_seeds(seeds.clone())
            .set_iterations(seeds.len())
    } else {
        builder
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
            .set_iterations(50)
    };

    let report = builder.run();

    report.eprint();

    if !report.is_success() {
        std::process::exit(1);
    }
}
