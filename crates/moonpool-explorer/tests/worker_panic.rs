//! A worker whose run panics must not harm its sibling workers, and the
//! controller must keep the recipe of the job that panicked.
//!
//! Regression test for #451 (a panicking worker killed its siblings, and its
//! recipe was lost). Worker mode uses `fork()`, so this test must run in its
//! own process (nextest default).

#![cfg(unix)]

use std::time::Duration;

use moonpool_explorer::simulations::{rng_count, rng_reset, rng_set_breakpoints};
use moonpool_explorer::{ExplorationConfig, ExploreJob, Explorer, Recipe, set_rng_count_hook};

/// A slow job sleeps this long. A panicking job panics after
/// [`PANIC_AFTER`], while a slow sibling still runs.
const SLOW_RUN: Duration = Duration::from_millis(600);

/// Time before a panicking job panics.
const PANIC_AFTER: Duration = Duration::from_millis(100);

/// The jobs whose last reseed value is even panic. The others are slow and
/// pass.
fn panics(recipe: &Recipe) -> bool {
    recipe.last().is_some_and(|&(_, seed)| seed % 2 == 0)
}

fn run_job(root_seed: u64, job: &ExploreJob) -> bool {
    rng_reset(root_seed);
    rng_set_breakpoints(job.recipe.clone());
    if panics(&job.recipe) {
        std::thread::sleep(PANIC_AFTER);
        panic!("planted panic in an exploration job");
    }
    std::thread::sleep(SLOW_RUN);
    false
}

#[test]
fn panicking_worker_keeps_siblings_and_recipe() {
    let root_seed = 11;
    let jobs = 4;
    set_rng_count_hook(rng_count);
    let mut explorer = Explorer::new(ExplorationConfig {
        workers: 2,
        // The root run counts against this budget.
        max_runs_per_seed: jobs + 1,
        branching_factor: 4,
        max_frontier: 64,
        max_recipe_len: 8,
    })
    .expect("explorer init");
    explorer.begin_seed(root_seed);

    // The root run makes one discovery at draw count 0. The controller
    // schedules continuations (fresh reseeds) from it.
    rng_reset(root_seed);
    moonpool_assertions::assertion_bool(
        moonpool_assertions::AssertKind::Sometimes,
        true,
        true,
        "worker panic entry",
    );
    explorer.observe_root_run(false);

    explorer.explore(|job| run_job(root_seed, job));

    let stats = explorer.seed_stats();
    let bug_recipes = explorer.bug_recipes().to_vec();
    drop(explorer);
    moonpool_explorer::cleanup_assertions();
    moonpool_assertions::clear_discovery_hooks();

    // Every job ran to its end. Before the fix, the panicking worker killed
    // its slow sibling, so the sibling was counted as a bug.
    assert_eq!(stats.total_timelines, jobs + 1, "stats: {stats:?}");
    assert!(stats.bug_found >= 1, "no job panicked; stats: {stats:?}");
    assert!(
        stats.bug_found < jobs,
        "no slow job ran next to a panicking job; stats: {stats:?}"
    );
    // Each bug is a job that panicked, recorded with its own recipe. A slow
    // job is never a bug.
    assert_eq!(
        u64::try_from(bug_recipes.len()).expect("small count"),
        stats.bug_found,
        "recipes: {bug_recipes:?}"
    );
    for recipe in &bug_recipes {
        assert!(
            panics(recipe),
            "a slow job was recorded as a bug: {recipe:?}"
        );
    }
}
