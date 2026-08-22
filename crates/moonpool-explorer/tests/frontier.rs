//! Integration tests for the frontier controller.
//!
//! These drive the full exploration loop against the synthetic floor-ladder
//! scenario (see `moonpool_explorer::simulations`). Since worker mode uses
//! `fork()`, each test must run in its own process (nextest default).

use moonpool_explorer::ExplorationConfig;
use moonpool_explorer::simulations::{
    LADDER_FLOORS, ladder_replay_reproduces, run_frontier_ladder,
};

fn ladder_config(workers: usize) -> ExplorationConfig {
    ExplorationConfig {
        workers,
        max_runs_per_seed: 2000,
        branching_factor: 4,
        max_frontier: 256,
        max_recipe_len: 32,
    }
}

/// In-process mode climbs the whole ladder within budget: deep progress is
/// achievable with exactly one live process.
#[test]
fn in_process_reaches_deep_floor() {
    let outcome = run_frontier_ladder(42, ladder_config(0)).expect("scenario runs");

    assert!(outcome.stats.total_timelines <= 2000, "budget exceeded");
    assert_eq!(
        outcome.stats.max_active_workers, 0,
        "in-process mode must not fork workers"
    );
    assert_eq!(
        u64::try_from(outcome.deepest_floor).expect("floor is non-negative"),
        LADDER_FLOORS,
        "expected the ladder to be fully climbed (stats: {:?})",
        outcome.stats
    );
    assert!(outcome.stats.expansions > 0, "no productive expansions");
    assert!(
        outcome.tracked_states >= usize::try_from(LADDER_FLOORS).expect("small const"),
        "each floor should register a semantic state"
    );
}

/// In-process exploration is fully deterministic: identical inputs produce
/// identical statistics and identical bug recipes.
#[test]
fn in_process_exploration_is_deterministic() {
    let first = run_frontier_ladder(7, ladder_config(0)).expect("scenario runs");
    let second = run_frontier_ladder(7, ladder_config(0)).expect("scenario runs");

    assert_eq!(first.stats.total_timelines, second.stats.total_timelines);
    assert_eq!(first.stats.expansions, second.stats.expansions);
    assert_eq!(first.stats.discoveries, second.stats.discoveries);
    assert_eq!(first.stats.bug_found, second.stats.bug_found);
    assert_eq!(first.bug_recipes, second.bug_recipes);
    assert_eq!(first.deepest_floor, second.deepest_floor);
}

/// Worker mode keeps the process count strictly bounded while still making
/// deep progress, and every captured bug recipe replays deterministically.
#[test]
fn workers_are_bounded_and_recipes_replay() {
    let workers = 3;
    let outcome = run_frontier_ladder(42, ladder_config(workers)).expect("scenario runs");

    assert!(
        outcome.stats.max_active_workers <= workers,
        "worker bound exceeded: {} > {workers}",
        outcome.stats.max_active_workers
    );
    assert!(
        outcome.stats.max_active_workers > 0,
        "worker mode should have forked at least one worker"
    );
    assert!(outcome.stats.total_timelines <= 2000, "budget exceeded");
    assert_eq!(
        u64::try_from(outcome.deepest_floor).expect("floor is non-negative"),
        LADDER_FLOORS,
        "expected the ladder to be fully climbed (stats: {:?})",
        outcome.stats
    );

    let recipe = outcome
        .bug_recipes
        .first()
        .expect("reaching the final floor must capture a bug recipe");
    assert!(
        ladder_replay_reproduces(42, recipe),
        "bug recipe must reproduce the failure on replay: {}",
        moonpool_explorer::format_timeline(recipe)
    );
}

/// A single worker behaves like in-process exploration with fork isolation:
/// bounded to one live worker, same deep progress.
#[test]
fn single_worker_explores() {
    let outcome = run_frontier_ladder(42, ladder_config(1)).expect("scenario runs");
    assert!(outcome.stats.max_active_workers <= 1);
    assert!(
        outcome.deepest_floor >= 4,
        "one worker should still climb (deepest: {})",
        outcome.deepest_floor
    );
}
