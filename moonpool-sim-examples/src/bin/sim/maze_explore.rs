//! Binary target for maze exploration simulation.
//!
//! Runs the maze workload with fork-based exploration, producing coverage
//! data visible to sancov instrumentation.

use std::process;

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    let report = moonpool_sim::simulations::run_simulation(
        moonpool_sim::SimulationBuilder::new()
            .workload(moonpool_sim_examples::maze::MazeWorkload::default())
            .enable_exploration(moonpool_sim::ExplorationConfig {
                max_depth: 30,
                timelines_per_split: 4,
                global_energy: 20_000,
                adaptive: Some(moonpool_sim::AdaptiveConfig {
                    batch_size: 20,
                    min_timelines: 60,
                    max_timelines: 150,
                    per_mark_energy: 600,
                    warm_min_timelines: Some(20),
                }),
                parallelism: None,
            })
            .set_iterations(2),
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
