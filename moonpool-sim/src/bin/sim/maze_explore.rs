//! Binary target for maze exploration simulation.
//!
//! Runs the maze workload with fork-based exploration, producing coverage
//! data visible to sancov instrumentation.

use std::process;

fn main() {
    tracing_subscriber::fmt::init();

    let report = moonpool_sim::simulations::run_simulation(
        moonpool_sim::SimulationBuilder::new()
            .workload(moonpool_sim::simulations::maze::MazeWorkload::default())
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

    eprintln!("{report}");

    if let Some(ref exploration) = report.exploration {
        if exploration.total_timelines == 0 {
            eprintln!("ERROR: no timelines explored");
            process::exit(1);
        }
        eprintln!(
            "Exploration: {} timelines, {} bugs found",
            exploration.total_timelines, exploration.bugs_found
        );
    }
}
