//! Binary target for dungeon exploration simulation.
//!
//! Runs the dungeon workload with fork-based exploration, producing coverage
//! data visible to sancov instrumentation.

use std::process;

fn main() {
    tracing_subscriber::fmt::init();

    let report = moonpool_sim::simulations::run_simulation(
        moonpool_sim::SimulationBuilder::new()
            .workload(moonpool_sim::simulations::dungeon::DungeonWorkload::default())
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
