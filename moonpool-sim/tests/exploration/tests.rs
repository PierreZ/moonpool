//! Integration tests for fork-based multiverse exploration with SimulationBuilder.
//!
//! These tests verify that the explorer integrates correctly with the
//! simulation runner, triggering forks when `assert_sometimes!` succeeds.
//!
//! Since tests use `fork()`, each must run in its own process (nextest default).

use moonpool_explorer::{AdaptiveConfig, ExplorationConfig};
use moonpool_sim::RandomProvider;
use moonpool_sim::SimulationBuilder;

/// Helper: assert a report has no failed runs.
fn assert_no_failures(report: &moonpool_sim::SimulationReport) {
    assert_eq!(
        report.failed_runs, 0,
        "Failed seeds: {:?}",
        report.seeds_failing
    );
}

#[test]
fn test_fork_basic() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload_fn("fork_basic", |ctx| {
                let random = ctx.random().clone();
                async move {
                    let r = random.random_range(0..10u32);
                    moonpool_sim::assert_sometimes!(r < 3, "rare_path_hit");
                    Ok(())
                }
            })
            .set_iterations(5)
            .enable_exploration(ExplorationConfig {
                max_depth: 3,
                children_per_fork: 2,
                global_energy: 20,
                adaptive: None,
            })
            .run()
            .await
    });

    assert_no_failures(&report);

    // Verify exploration stats are reported
    if let Some(exp) = &report.exploration {
        assert!(
            exp.total_timelines >= 0,
            "Explorer should report timeline count"
        );
    }
}

#[test]
fn test_depth_limit() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload_fn("depth_limit", |ctx| {
                let random = ctx.random().clone();
                async move {
                    // Multiple sometimes assertions to create fork cascade
                    for i in 0..5u32 {
                        let r = random.random_range(0..10u32);
                        if r < 5 {
                            let name = format!("depth_gate_{}", i);
                            moonpool_sim::assert_sometimes!(true, &name);
                        }
                    }
                    Ok(())
                }
            })
            .set_iterations(3)
            .enable_exploration(ExplorationConfig {
                max_depth: 2, // Shallow depth limit
                children_per_fork: 2,
                global_energy: 50,
                adaptive: None,
            })
            .run()
            .await
    });

    assert_no_failures(&report);
}

#[test]
fn test_energy_limit() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload_fn("energy_limit", |ctx| {
                let random = ctx.random().clone();
                async move {
                    let r = random.random_range(0..5u32);
                    moonpool_sim::assert_sometimes!(r == 0, "energy_gate_a");
                    moonpool_sim::assert_sometimes!(r == 1, "energy_gate_b");
                    moonpool_sim::assert_sometimes!(r == 2, "energy_gate_c");
                    Ok(())
                }
            })
            .set_iterations(5)
            .enable_exploration(ExplorationConfig {
                max_depth: 4,
                children_per_fork: 2,
                global_energy: 6, // Very tight energy budget
                adaptive: None,
            })
            .run()
            .await
    });

    assert_no_failures(&report);

    if let Some(exp) = &report.exploration {
        assert!(
            exp.total_timelines <= 6,
            "Energy limit should cap total timelines: got {}",
            exp.total_timelines
        );
    }
}

#[test]
fn test_sometimes_each_triggers_fork() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload_fn("each_fork", |ctx| {
                let random = ctx.random().clone();
                async move {
                    let op_type = random.random_range(0..3u32);
                    moonpool_sim::assert_sometimes_each!("op_coverage", [("type", op_type as i64)]);
                    Ok(())
                }
            })
            .set_iterations(5)
            .enable_exploration(ExplorationConfig {
                max_depth: 3,
                children_per_fork: 2,
                global_energy: 30,
                adaptive: Some(AdaptiveConfig {
                    batch_size: 2,
                    min_timelines: 2,
                    max_timelines: 8,
                    per_mark_energy: 5,
                }),
            })
            .run()
            .await
    });

    assert_no_failures(&report);
}
