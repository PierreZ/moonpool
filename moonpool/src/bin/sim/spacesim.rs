//! Binary target for cargo hauling network simulation.

use std::rc::Rc;

use moonpool::actors::{
    ActorDirectory, ActorStateStore, ClusterConfig, InMemoryDirectory, InMemoryStateStore,
    SharedMembership,
};
use moonpool::simulations::invariants::DirectoryConsistency;
use moonpool::simulations::spacesim::{
    invariants::{CargoConservation, NonNegativeInventory},
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

    let stations = [
        "alpha-mine",
        "alpha-dock",
        "beta-fab",
        "beta-dock",
        "gamma-refinery",
        "gamma-dock",
    ];
    let ships = ["hauler-1", "hauler-2", "hauler-3", "hauler-4"];

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
        .invariant(CargoConservation)
        .invariant(NonNegativeInventory)
        .invariant(DirectoryConsistency);

    let builder = if let Some(ref seeds) = debug_seeds {
        builder
            .set_debug_seeds(seeds.clone())
            .set_iterations(seeds.len())
    } else {
        builder.set_iterations(50)
    };

    let report = builder.run();

    report.eprint();

    if !report.is_success() {
        std::process::exit(1);
    }
}
