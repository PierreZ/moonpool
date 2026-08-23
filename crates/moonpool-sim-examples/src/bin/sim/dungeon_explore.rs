//! Binary target for dungeon exploration simulation.
//!
//! Runs the dungeon workload under the frontier explorer, producing coverage
//! data visible to sancov instrumentation. Process usage stays bounded at
//! 1 controller + `workers` short-lived worker processes, however deep the
//! dungeon exploration goes.

use std::process;

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = moonpool_sim::SimulationBuilder::new()
        .workload_factory(|| Box::new(moonpool_sim_examples::dungeon::DungeonWorkload::default()))
        .enable_exploration(moonpool_sim::ExplorationConfig {
            workers: 4,
            max_runs_per_seed: 24_000,
            branching_factor: 4,
            max_frontier: 1024,
            max_recipe_len: 64,
        })
        .set_iterations(3)
        .run();

    report.eprint();

    if report
        .exploration
        .as_ref()
        .is_some_and(|e| e.total_timelines == 0)
    {
        eprintln!("ERROR: no timelines explored");
        process::exit(1);
    }
}
