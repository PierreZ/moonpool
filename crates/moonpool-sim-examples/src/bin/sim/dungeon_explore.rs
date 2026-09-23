//! Binary target for dungeon exploration simulation.
//!
//! Runs the dungeon workload under the frontier explorer, producing coverage
//! data visible to sancov instrumentation. Process usage stays bounded at
//! 1 controller + `workers` short-lived worker processes, however deep the
//! dungeon exploration goes.

/// The shallowest dungeon level a healthy exploration must reach when it does
/// not find the treasure outright. Runs at this budget reach levels 7 to 10 of
/// the 10-level dungeon, so a stall at or before level 4 is a regression in
/// the explorer's guidance, not seed variance.
const LEVEL_FLOOR: i64 = 5;

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
        .run()
        .expect("simulation configuration is valid");

    moonpool_sim_examples::support::finish_or_exit_below_watermark(
        &report,
        "dungeon level reached",
        LEVEL_FLOOR,
    );
}
