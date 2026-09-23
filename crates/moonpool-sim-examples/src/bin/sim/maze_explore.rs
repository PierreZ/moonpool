//! Binary target for maze exploration simulation.
//!
//! Runs the maze workload under the frontier explorer, producing coverage
//! data visible to sancov instrumentation.

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = moonpool_sim::SimulationBuilder::new()
        .workload_factory(|| Box::new(moonpool_sim_examples::maze::MazeWorkload::default()))
        .enable_exploration(moonpool_sim::ExplorationConfig {
            workers: 0,
            max_runs_per_seed: 600,
            branching_factor: 4,
            max_frontier: 512,
            max_recipe_len: 32,
        })
        .set_iterations(2)
        .run()
        .expect("simulation configuration is valid");

    moonpool_sim_examples::support::finish_or_exit_if_unexplored(&report);
}
