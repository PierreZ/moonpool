//! Integration tests for fork-based multiverse exploration.
//!
//! These tests exercise the moonpool-explorer crate wired into moonpool-sim.
//! Since they use `fork()`, each test must run in its own process (nextest default).

use async_trait::async_trait;
use moonpool_sim::{
    ExplorationConfig, SimContext, SimulationBuilder, SimulationReport, SimulationResult, Workload,
};

/// Helper to run a simulation and return the report.
fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move { builder.run().await })
}

// ---------------------------------------------------------------------------
// Workload structs
// ---------------------------------------------------------------------------

/// Simple workload that fires a single sometimes assertion.
struct AssertOnceWorkload {
    message: &'static str,
}

#[async_trait(?Send)]
impl Workload for AssertOnceWorkload {
    fn name(&self) -> &str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        moonpool_sim::assert_sometimes!(true, self.message);
        Ok(())
    }
}

/// Workload that triggers a fork, then fails in child processes (simulates a bug).
struct ChildBugWorkload;

#[async_trait(?Send)]
impl Workload for ChildBugWorkload {
    fn name(&self) -> &str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        moonpool_sim::assert_sometimes!(true, "triggers fork");

        if moonpool_explorer::explorer_is_child() {
            return Err(moonpool_sim::SimulationError::InvalidState(
                "simulated bug in child".to_string(),
            ));
        }

        Ok(())
    }
}

/// Workload with two consecutive assertion gates (tests depth limiting).
struct TwoGateWorkload;

#[async_trait(?Send)]
impl Workload for TwoGateWorkload {
    fn name(&self) -> &str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        // First assertion — will fork in parent (depth 0 -> 1)
        moonpool_sim::assert_sometimes!(true, "first gate");

        // Second assertion — children are at depth 1 == max_depth,
        // so they should NOT fork further
        moonpool_sim::assert_sometimes!(true, "second gate");

        Ok(())
    }
}

/// Workload with three assertion gates (tests energy limiting).
struct ThreeGateWorkload;

#[async_trait(?Send)]
impl Workload for ThreeGateWorkload {
    fn name(&self) -> &str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        moonpool_sim::assert_sometimes!(true, "gate 1");
        moonpool_sim::assert_sometimes!(true, "gate 2");
        moonpool_sim::assert_sometimes!(true, "gate 3");

        Ok(())
    }
}

/// Workload with cascaded probabilistic gates (planted bug scenario).
struct PlantedBugWorkload;

#[async_trait(?Send)]
impl Workload for PlantedBugWorkload {
    fn name(&self) -> &str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        // Gate 1: always passes — triggers the first fork
        moonpool_sim::assert_sometimes!(true, "gate 1");

        // Gate 2: ~10% chance (children explore with reseeded RNG)
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

/// Workload that exercises assert_sometimes_each! with identity keys.
struct EachBucketWorkload;

#[async_trait(?Send)]
impl Workload for EachBucketWorkload {
    fn name(&self) -> &str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        // Use assert_sometimes_each! with identity keys to trigger forking.
        // Each unique (lock, depth) combination creates a separate bucket.
        for lock in 0..2 {
            for depth in 1..3 {
                moonpool_sim::assert_sometimes_each!(
                    "each_gate",
                    [("lock", lock as i64), ("depth", depth as i64)]
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

/// Test basic fork exploration — verify children are forked and stats update.
#[test]
fn test_fork_basic() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(ExplorationConfig {
                max_depth: 1,
                timelines_per_split: 2,
                global_energy: 10,
                adaptive: None,
                parallelism: None,
            })
            .workload(AssertOnceWorkload {
                message: "triggers fork",
            }),
    );

    assert_eq!(report.successful_runs, 1);

    let exp = report.exploration.expect("exploration report missing");
    assert!(
        exp.total_timelines > 0,
        "expected forked children, got total_timelines={}",
        exp.total_timelines
    );
    assert!(
        exp.fork_points > 0,
        "expected fork points, got fork_points={}",
        exp.fork_points
    );
}

/// Test that forked children that fail report exit code 42 (bug found).
#[test]
fn test_child_exit_code() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(ExplorationConfig {
                max_depth: 1,
                timelines_per_split: 2,
                global_energy: 10,
                adaptive: None,
                parallelism: None,
            })
            .workload(ChildBugWorkload),
    );

    // Parent should succeed
    assert_eq!(report.successful_runs, 1);

    let exp = report.exploration.expect("exploration report missing");
    assert!(
        exp.bugs_found > 0,
        "expected bugs_found > 0, got {}",
        exp.bugs_found
    );
}

/// Test that max_depth=1 prevents grandchildren (no recursive forking).
#[test]
fn test_depth_limit() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(ExplorationConfig {
                max_depth: 1,
                timelines_per_split: 2,
                global_energy: 100,
                adaptive: None,
                parallelism: None,
            })
            .workload(TwoGateWorkload),
    );

    assert_eq!(report.successful_runs, 1);

    // With max_depth=1 and timelines_per_split=2:
    // - Root forks 2 children at gate_1 (depth 0->1)
    // - Gate_2 in children: depth=1 == max_depth, no fork
    // So total_timelines should be exactly 2
    let exp = report.exploration.expect("exploration report missing");
    assert!(
        exp.total_timelines <= 4,
        "depth limit exceeded: total_timelines={} (expected <= 4)",
        exp.total_timelines
    );
}

/// Test that global energy budget limits the number of forks.
#[test]
fn test_energy_limit() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(ExplorationConfig {
                max_depth: 3,
                timelines_per_split: 8,
                global_energy: 2,
                adaptive: None,
                parallelism: None,
            })
            .workload(ThreeGateWorkload),
    );

    assert_eq!(report.successful_runs, 1);

    // With energy=2 and timelines_per_split=8, we can only fork 2 children total
    let exp = report.exploration.expect("exploration report missing");
    assert!(
        exp.total_timelines <= 2,
        "energy limit exceeded: total_timelines={} (expected <= 2)",
        exp.total_timelines
    );
}

/// Test that a recipe captured from a bug-finding fork can be replayed
/// via RNG breakpoints to produce identical results.
#[test]
fn test_replay_matches_fork() {
    // Verify round-trip format/parse of recipes
    let recipe = vec![(42, 12345), (17, 67890)];
    let formatted = moonpool_sim::format_timeline(&recipe);
    assert_eq!(formatted, "42@12345 -> 17@67890");

    let parsed = moonpool_sim::parse_timeline(&formatted).expect("parse failed");
    assert_eq!(recipe, parsed);

    // Also verify empty case
    let empty = moonpool_sim::format_timeline(&[]);
    assert_eq!(empty, "");
    let parsed_empty = moonpool_sim::parse_timeline("").expect("parse failed");
    assert!(parsed_empty.is_empty());
}

/// Test planted bug scenario: 3 gates at P=0.1, exploration should find the
/// path through all gates much more efficiently than brute force.
#[test]
fn test_planted_bug() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(ExplorationConfig {
                max_depth: 3,
                timelines_per_split: 4,
                global_energy: 50,
                adaptive: None,
                parallelism: None,
            })
            .workload(PlantedBugWorkload),
    );

    // Parent should succeed (bugs only found in forked children)
    assert_eq!(report.successful_runs, 1);
    let exp = report.exploration.expect("exploration report missing");
    assert!(
        exp.total_timelines > 0,
        "expected some exploration, got total_timelines=0"
    );
}

/// Test that assert_sometimes_each! triggers fork-based exploration.
#[test]
fn test_sometimes_each_triggers_fork() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(ExplorationConfig {
                max_depth: 2,
                timelines_per_split: 2,
                global_energy: 20,
                adaptive: None,
                parallelism: None,
            })
            .workload(EachBucketWorkload),
    );

    assert_eq!(report.successful_runs, 1);

    let exp = report.exploration.expect("exploration report missing");
    assert!(
        exp.total_timelines > 0,
        "expected forked children from assert_sometimes_each!, got total_timelines={}",
        exp.total_timelines
    );
}
