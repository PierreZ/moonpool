//! Integration tests for frontier-based exploration.
//!
//! These exercise the moonpool-explorer controller wired into moonpool-sim.
//! Since worker mode uses `fork()`, each test must run in its own process
//! (nextest default).

use async_trait::async_trait;
use moonpool_sim::{
    ExplorationConfig, SimContext, SimulationBuilder, SimulationReport, SimulationResult, Workload,
};

/// Helper to run a simulation and return the report.
fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    builder.run()
}

/// Small bounded exploration config for tests.
fn test_config(workers: usize, max_runs: u64) -> ExplorationConfig {
    ExplorationConfig {
        workers,
        max_runs_per_seed: max_runs,
        branching_factor: 2,
        max_frontier: 64,
        max_recipe_len: 8,
    }
}

// ---------------------------------------------------------------------------
// Workload structs
// ---------------------------------------------------------------------------

/// Simple workload that fires a single sometimes assertion.
struct AssertOnceWorkload {
    message: &'static str,
}

#[async_trait]
impl Workload for AssertOnceWorkload {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        moonpool_sim::assert_sometimes!(true, self.message);
        Ok(())
    }
}

/// Workload that succeeds in the root run but fails in exploration workers
/// (simulates a bug found only in an explored timeline).
struct ChildBugWorkload;

#[async_trait]
impl Workload for ChildBugWorkload {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        moonpool_sim::assert_sometimes!(true, "triggers exploration");

        if moonpool_explorer::explorer_is_child() {
            return Err(moonpool_sim::SimulationError::InvalidState(
                "simulated bug in explored timeline".to_string(),
            ));
        }

        Ok(())
    }
}

/// Workload with cascaded probabilistic gates (planted bug scenario).
struct PlantedBugWorkload;

#[async_trait]
impl Workload for PlantedBugWorkload {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        // Gate 1: always passes — the first discovery.
        moonpool_sim::assert_sometimes!(true, "gate 1");

        // Gate 2: ~10% chance (continuations re-roll with fresh seeds).
        let gate2 = moonpool_sim::sim_random_range(0u32..10) == 0;
        moonpool_sim::assert_sometimes!(gate2, "gate 2");

        if gate2 {
            // Gate 3: ~10% chance
            let gate3 = moonpool_sim::sim_random_range(0u32..10) == 0;
            moonpool_sim::assert_sometimes!(gate3, "gate 3");

            if gate3 {
                // Bug! Both gates passed
                return Err(moonpool_sim::SimulationError::InvalidState(
                    "planted bug found: all gates passed".to_string(),
                ));
            }
        }

        Ok(())
    }
}

/// Workload that exercises `assert_sometimes_each`! with identity keys.
struct EachBucketWorkload;

#[async_trait]
impl Workload for EachBucketWorkload {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        // Each unique (lock, depth) combination creates a separate bucket,
        // and each new bucket is a discovery the controller can anchor to.
        for lock in 0..2 {
            for depth in 1..3 {
                moonpool_sim::assert_sometimes_each!(
                    "each_gate",
                    [("lock", i64::from(lock)), ("depth", i64::from(depth))]
                );
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test that exploration is disabled by default and old behavior is unchanged.
#[test]
fn test_exploration_disabled_default() {
    let report = run_simulation(SimulationBuilder::new().set_iterations(1).workload(
        AssertOnceWorkload {
            message: "always passes",
        },
    ));

    assert_eq!(report.successful_runs, 1);
    assert!(!moonpool_explorer::explorer_is_child());
    assert!(
        report.exploration.is_none(),
        "exploration report should be None when disabled"
    );
}

/// Basic exploration: discoveries produce expansions and extra timelines,
/// and the run budget is respected.
#[test]
fn test_exploration_basic() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(test_config(0, 10))
            .workload(AssertOnceWorkload {
                message: "triggers exploration",
            }),
    );

    assert_eq!(report.successful_runs, 1);

    let exp = report.exploration.expect("exploration report missing");
    assert!(
        exp.total_timelines > 1,
        "expected exploration runs beyond the root, got total_timelines={}",
        exp.total_timelines
    );
    assert!(
        exp.total_timelines <= 10,
        "run budget exceeded: total_timelines={}",
        exp.total_timelines
    );
    assert!(
        exp.expansions > 0,
        "expected at least one productive expansion"
    );
    assert!(
        exp.discoveries > 0,
        "expected the sometimes assertion to be discovered"
    );
}

/// Exploration runs that fail are counted as bugs and captured as recipes.
/// Uses a single worker: `ChildBugWorkload` fails only in forked workers,
/// which also covers the workers = 1 configuration.
#[test]
fn test_bug_capture_and_recipe() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(test_config(1, 10))
            .workload(ChildBugWorkload),
    );

    // The root (in-process) run succeeds; bugs appear in exploration runs.
    assert_eq!(report.successful_runs, 1);

    let exp = report.exploration.expect("exploration report missing");
    assert!(
        exp.bugs_found > 0,
        "expected bugs_found > 0, got {}",
        exp.bugs_found
    );
    assert!(
        !exp.bug_recipes.is_empty(),
        "expected bug recipes to be captured"
    );
    let recipe = &exp.bug_recipes[0];
    assert!(
        !recipe.recipe.is_empty(),
        "bug recipe should contain at least one replay segment"
    );
}

/// Worker mode: forked workers stay within the configured bound, and bugs
/// found in workers are captured.
#[test]
fn test_workers_bounded() {
    let workers = 2;
    let mut config = test_config(workers, 12);
    config.branching_factor = 3;
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(config)
            .workload(ChildBugWorkload),
    );

    assert_eq!(report.successful_runs, 1);
    let exp = report.exploration.expect("exploration report missing");
    assert!(
        exp.max_active_workers <= workers,
        "worker bound exceeded: {} > {workers}",
        exp.max_active_workers
    );
    assert!(
        exp.max_active_workers > 0,
        "worker mode should have forked workers"
    );
    assert!(exp.total_timelines <= 12, "run budget exceeded");
    assert!(exp.bugs_found > 0, "expected worker bugs to be captured");
}

/// In-process exploration (workers = 0) is fully deterministic: two identical
/// runs produce identical exploration statistics and recipes.
#[test]
fn test_in_process_exploration_deterministic() {
    let run = || {
        run_simulation(
            SimulationBuilder::new()
                .set_iterations(2)
                .set_debug_seeds(vec![1111, 2222])
                .enable_exploration(test_config(0, 8))
                .workload(PlantedBugWorkload),
        )
    };
    let first = run();
    let second = run();

    let exp1 = first.exploration.expect("exploration report missing");
    let exp2 = second.exploration.expect("exploration report missing");
    assert_eq!(exp1.total_timelines, exp2.total_timelines);
    assert_eq!(exp1.expansions, exp2.expansions);
    assert_eq!(exp1.discoveries, exp2.discoveries);
    assert_eq!(exp1.bugs_found, exp2.bugs_found);
    assert_eq!(exp1.bug_recipes, exp2.bug_recipes);
    assert_eq!(exp1.per_seed_timelines, exp2.per_seed_timelines);
}

/// Recipe round-trip formatting (replay input format).
#[test]
fn test_recipe_format_roundtrip() {
    let recipe = vec![(42, 12345), (17, 67890)];
    let formatted = moonpool_sim::format_timeline(&recipe);
    assert_eq!(formatted, "42@12345 -> 17@67890");

    let parsed = moonpool_sim::parse_timeline(&formatted).expect("parse failed");
    assert_eq!(recipe, parsed);

    let empty = moonpool_sim::format_timeline(&[]);
    assert_eq!(empty, "");
    let parsed_empty = moonpool_sim::parse_timeline("").expect("parse failed");
    assert!(parsed_empty.is_empty());
}

/// Planted-bug scenario: cascaded rare gates are found through recipe
/// extension, and the captured recipe replays to the same failure through
/// [`SimulationBuilder::replay_timeline`].
#[test]
fn test_planted_bug_recipe_replays() {
    let mut config = test_config(0, 300);
    config.branching_factor = 4;
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .set_debug_seeds(vec![4242])
            .enable_exploration(config)
            .workload(PlantedBugWorkload),
    );

    let exp = report.exploration.expect("exploration report missing");
    assert!(
        exp.total_timelines > 1,
        "expected exploration beyond the root run"
    );
    let bug = exp
        .bug_recipes
        .first()
        .expect("300 guided runs should find the ~1% planted bug");
    assert!(
        !bug.recipe.is_empty(),
        "planted bug recipe should have replay segments"
    );

    // Replaying the recipe must reproduce the exact failing timeline.
    let replay = run_simulation(
        SimulationBuilder::new()
            .replay_timeline(bug.seed, bug.recipe.clone())
            .workload(PlantedBugWorkload),
    );
    assert_eq!(
        replay.failed_runs, 1,
        "replayed timeline must reproduce the planted bug"
    );
}

/// `assert_sometimes_each!` buckets are discoveries: each new bucket
/// contributes to the journal and drives expansions.
#[test]
fn test_sometimes_each_drives_exploration() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(test_config(0, 10))
            .workload(EachBucketWorkload),
    );

    assert_eq!(report.successful_runs, 1);

    let exp = report.exploration.expect("exploration report missing");
    assert!(
        exp.discoveries >= 4,
        "expected all four (lock, depth) buckets to be discovered, got {}",
        exp.discoveries
    );
    assert!(exp.expansions > 0, "expected bucket-driven expansions");
}
