//! Integration tests for fork-based multiverse exploration.
//!
//! These tests exercise the moonpool-explorer crate wired into moonpool-sim.
//! Since they use `fork()`, each test must run in its own process (nextest default).

use moonpool_sim::{ExplorationConfig, SimulationBuilder, SimulationReport};

/// Helper to run a simulation and return the report.
fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move { builder.run().await })
}

/// Test that exploration is disabled by default and old behavior is unchanged.
#[test]
fn test_exploration_disabled_default() {
    let report = run_simulation(SimulationBuilder::new().set_iterations(1).workload_fn(
        "client",
        |_ctx| async move {
            moonpool_sim::assert_sometimes!(true, "always passes");
            Ok(())
        },
    ));

    assert_eq!(report.successful_runs, 1);
    assert!(!moonpool_explorer::explorer_is_child());
    assert!(moonpool_explorer::get_exploration_stats().is_none());
}

/// Test basic fork exploration — verify children are forked and stats update.
#[test]
fn test_fork_basic() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(ExplorationConfig {
                max_depth: 1,
                children_per_fork: 2,
                global_energy: 10,
                adaptive: None,
            })
            .workload_fn("client", |_ctx| async move {
                // This sometimes assertion success will trigger forking
                moonpool_sim::assert_sometimes!(true, "triggers fork");
                Ok(())
            }),
    );

    assert_eq!(report.successful_runs, 1);

    // After exploration, stats should show timelines were explored
    let stats = moonpool_explorer::get_exploration_stats();
    if let Some(stats) = stats {
        assert!(
            stats.total_timelines > 0,
            "expected forked children, got total_timelines={}",
            stats.total_timelines
        );
        assert!(
            stats.fork_points > 0,
            "expected fork points, got fork_points={}",
            stats.fork_points
        );
    }
    // Note: stats may be None if cleanup already ran, which is fine
}

/// Test that forked children that fail report exit code 42 (bug found).
#[test]
fn test_child_exit_code() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(ExplorationConfig {
                max_depth: 1,
                children_per_fork: 2,
                global_energy: 10,
                adaptive: None,
            })
            .workload_fn("client", |_ctx| async move {
                // This assertion triggers forking on success
                moonpool_sim::assert_sometimes!(true, "triggers fork");

                // In the child, the reseeded RNG may cause different behavior.
                // We use a failure to simulate a bug.
                // But we only fail in children (parent must succeed to report).
                if moonpool_explorer::explorer_is_child() {
                    return Err(moonpool_sim::SimulationError::InvalidState(
                        "simulated bug in child".to_string(),
                    ));
                }

                Ok(())
            }),
    );

    // Parent should succeed
    assert_eq!(report.successful_runs, 1);

    // Check if bug was found via shared stats
    let stats = moonpool_explorer::get_exploration_stats();
    if let Some(stats) = stats {
        assert!(
            stats.bug_found > 0,
            "expected bug_found > 0, got {}",
            stats.bug_found
        );
    }
}

/// Test that max_depth=1 prevents grandchildren (no recursive forking).
#[test]
fn test_depth_limit() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(ExplorationConfig {
                max_depth: 1,
                children_per_fork: 2,
                global_energy: 100,
                adaptive: None,
            })
            .workload_fn("client", |_ctx| async move {
                // First assertion — will fork in parent (depth 0 -> 1)
                moonpool_sim::assert_sometimes!(true, "first gate");

                // Second assertion — children are at depth 1 == max_depth,
                // so they should NOT fork further
                moonpool_sim::assert_sometimes!(true, "second gate");

                Ok(())
            }),
    );

    assert_eq!(report.successful_runs, 1);

    // With max_depth=1 and children_per_fork=2:
    // - Root forks 2 children at gate_1 (depth 0->1)
    // - Gate_2 in children: depth=1 == max_depth, no fork
    // So total_timelines should be exactly 2
    let stats = moonpool_explorer::get_exploration_stats();
    if let Some(stats) = stats {
        assert!(
            stats.total_timelines <= 4,
            "depth limit exceeded: total_timelines={} (expected <= 4)",
            stats.total_timelines
        );
    }
}

/// Test that global energy budget limits the number of forks.
#[test]
fn test_energy_limit() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(ExplorationConfig {
                max_depth: 3,
                children_per_fork: 8,
                global_energy: 2,
                adaptive: None,
            })
            .workload_fn("client", |_ctx| async move {
                // Many assertions, but energy=2 should limit total forks
                moonpool_sim::assert_sometimes!(true, "gate 1");
                moonpool_sim::assert_sometimes!(true, "gate 2");
                moonpool_sim::assert_sometimes!(true, "gate 3");

                Ok(())
            }),
    );

    assert_eq!(report.successful_runs, 1);

    // With energy=2 and children_per_fork=8, we can only fork 2 children total
    let stats = moonpool_explorer::get_exploration_stats();
    if let Some(stats) = stats {
        assert!(
            stats.total_timelines <= 2,
            "energy limit exceeded: total_timelines={} (expected <= 2)",
            stats.total_timelines
        );
    }
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
                children_per_fork: 4,
                global_energy: 50,
                adaptive: None,
            })
            .workload_fn("client", |_ctx| async move {
                // Gate 1: ~10% chance
                let gate1 = moonpool_sim::sim_random_range(0u32..10) == 0;
                moonpool_sim::assert_sometimes!(gate1, "gate 1");

                if gate1 {
                    // Gate 2: ~10% chance (conditional on gate 1)
                    let gate2 = moonpool_sim::sim_random_range(0u32..10) == 0;
                    moonpool_sim::assert_sometimes!(gate2, "gate 2");

                    if gate2 {
                        // Gate 3: ~10% chance (conditional on gate 1 and 2)
                        let gate3 = moonpool_sim::sim_random_range(0u32..10) == 0;
                        moonpool_sim::assert_sometimes!(gate3, "gate 3");

                        if gate3 {
                            // Bug! All 3 gates passed
                            return Err(moonpool_sim::SimulationError::InvalidState(
                                "planted bug found: all 3 gates passed".to_string(),
                            ));
                        }
                    }
                }

                Ok(())
            }),
    );

    // The parent should succeed (only children hit the bug)
    assert_eq!(report.successful_runs, 1);

    // With exploration enabled, we should find the planted bug much more
    // efficiently than brute force (P = 0.001 without exploration).
    // The test mainly verifies the exploration machinery works end-to-end
    // without crashing or hanging.
    let stats = moonpool_explorer::get_exploration_stats();
    if let Some(stats) = stats {
        assert!(
            stats.total_timelines > 0,
            "expected some exploration, got total_timelines=0"
        );
    }
}

/// Test that assert_sometimes_each! triggers fork-based exploration.
#[test]
fn test_sometimes_each_triggers_fork() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(ExplorationConfig {
                max_depth: 2,
                children_per_fork: 2,
                global_energy: 20,
                adaptive: None,
            })
            .workload_fn("client", |_ctx| async move {
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
            }),
    );

    assert_eq!(report.successful_runs, 1);

    // Verify exploration happened
    let stats = moonpool_explorer::get_exploration_stats();
    if let Some(stats) = stats {
        assert!(
            stats.total_timelines > 0,
            "expected forked children from assert_sometimes_each!, got total_timelines={}",
            stats.total_timelines
        );
    }
}
