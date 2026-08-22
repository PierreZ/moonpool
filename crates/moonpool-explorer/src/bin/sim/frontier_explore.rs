//! Binary target for frontier-based exploration scenarios.
//!
//! Runs the floor-ladder scenario through the full controller loop — once
//! in-process (deterministic) and once with forked workers — and validates
//! resource bounds, deep progress, and bug-recipe replay. Built with sancov
//! instrumentation of `moonpool_explorer` under `cargo xtask sim run`, so it
//! also exercises the code-coverage pipeline on both Linux and macOS.

use std::process;

use moonpool_explorer::ExplorationConfig;
use moonpool_explorer::simulations::{
    LADDER_FLOORS, ladder_replay_reproduces, run_frontier_ladder,
};

fn fail(msg: &str) -> ! {
    eprintln!("FAILED: {msg}");
    process::exit(1);
}

fn run_scenario(name: &str, workers: usize) {
    eprintln!("=== Frontier ladder ({name}) ===");
    let config = ExplorationConfig {
        workers,
        max_runs_per_seed: 2000,
        branching_factor: 4,
        max_frontier: 256,
        max_recipe_len: 32,
    };
    let outcome = match run_frontier_ladder(42, config) {
        Ok(outcome) => outcome,
        Err(e) => fail(&format!("{e}")),
    };
    eprintln!(
        "  timelines={} expansions={} discoveries={} deepest_floor={} \
         max_workers={} bugs={}",
        outcome.stats.total_timelines,
        outcome.stats.expansions,
        outcome.stats.discoveries,
        outcome.deepest_floor,
        outcome.stats.max_active_workers,
        outcome.stats.bug_found,
    );

    if outcome.stats.total_timelines > 2000 {
        fail("run budget exceeded");
    }
    if outcome.stats.max_active_workers > workers {
        fail("worker bound exceeded");
    }
    let deepest = u64::try_from(outcome.deepest_floor).unwrap_or(0);
    if deepest < LADDER_FLOORS {
        fail(&format!(
            "expected the ladder to be climbed to floor {LADDER_FLOORS}, reached {deepest}"
        ));
    }
    let Some(recipe) = outcome.bug_recipes.first() else {
        fail("no bug recipe captured");
    };
    if !ladder_replay_reproduces(42, recipe) {
        fail("bug recipe did not reproduce the failure on replay");
    }
    eprintln!("PASSED\n");
}

fn main() {
    run_scenario("in-process", 0);
    run_scenario("4 workers", 4);
    eprintln!("All frontier scenarios passed.");
}
