//! Binary target for dungeon exploration simulation.
//!
//! Runs the dungeon workload with fork-based exploration, producing coverage
//! data visible to sancov instrumentation.

use std::process;

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    let report = moonpool_sim::simulations::run_simulation(
        moonpool_sim::SimulationBuilder::new()
            .workload(moonpool_sim_examples::dungeon::DungeonWorkload::default())
            .enable_exploration(moonpool_sim::ExplorationConfig {
                max_depth: 120,
                timelines_per_split: 4,
                global_energy: 400_000,
                adaptive: Some(moonpool_sim::AdaptiveConfig {
                    batch_size: 30,
                    min_timelines: 400,
                    max_timelines: 1000,
                    per_mark_energy: 10_000,
                    warm_min_timelines: Some(30),
                }),
                parallelism: Some(moonpool_sim::Parallelism::HalfCores),
            })
            .set_iterations(3),
    );

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
