//! Integration tests for assertion macros (assert_always!, assert_sometimes!,
//! assert_sometimes_each!) used with the SimulationBuilder.
//!
//! These tests verify that assertion macros integrate correctly with the
//! simulation runner and report assertion results.

use moonpool_sim::RandomProvider;
use moonpool_sim::SimulationBuilder;

#[test]
fn test_assert_always_success_in_simulation() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload_fn("always_pass", |_ctx| async move {
                moonpool_sim::assert_always!(1 + 1 == 2, "Basic arithmetic must hold");
                moonpool_sim::assert_always!(true, "Truth must be true");
                Ok(())
            })
            .set_iterations(3)
            .run()
            .await
    });

    assert_eq!(report.failed_runs, 0);
    assert_eq!(report.successful_runs, 3);
}

#[test]
fn test_assert_always_failure_causes_failed_run() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload_fn("always_fail", |_ctx| async move {
                moonpool_sim::assert_always!(false, "This will fail");
                Ok(())
            })
            .set_iterations(1)
            .run()
            .await
    });

    // The workload should have panicked, causing a failed run
    assert!(
        report.failed_runs > 0,
        "assert_always!(false) should cause a failed run"
    );
}

#[test]
fn test_assert_sometimes_tracking_in_simulation() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload_fn("sometimes_track", |_ctx| async move {
                // Use a deterministic value that exercises both branches
                moonpool_sim::assert_sometimes!(true, "Always true path");
                moonpool_sim::assert_sometimes!(false, "Never true path");
                Ok(())
            })
            .set_iterations(1)
            .run()
            .await
    });

    assert_eq!(report.failed_runs, 0);

    // Assertions should have been tracked from the last (only) iteration
    assert!(
        report.assertion_results.contains_key("Always true path"),
        "Assertion 'Always true path' should be tracked"
    );
    assert!(
        report.assertion_results.contains_key("Never true path"),
        "Assertion 'Never true path' should be tracked"
    );

    // Verify success tracking
    let always_true = &report.assertion_results["Always true path"];
    let never_true = &report.assertion_results["Never true path"];
    assert_eq!(
        always_true.successes, 1,
        "Always true should have 1 success"
    );
    assert_eq!(
        always_true.total_checks, 1,
        "Always true should have 1 check"
    );
    assert_eq!(
        never_true.successes, 0,
        "Never true should have 0 successes"
    );
    assert_eq!(never_true.total_checks, 1, "Never true should have 1 check");
}

#[test]
fn test_assert_sometimes_each_basic_in_simulation() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload_fn("sometimes_each", |ctx| {
                let random = ctx.random().clone();
                async move {
                    let r = random.random_range(0..3u32);
                    // Each unique value of r creates a separate bucket
                    moonpool_sim::assert_sometimes_each!("op_type", [("type", r as i64)]);
                    Ok(())
                }
            })
            .set_iterations(10)
            .run()
            .await
    });

    assert_eq!(report.failed_runs, 0);
}

#[test]
fn test_assert_sometimes_each_quality_watermark() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload_fn("sometimes_each_quality", |ctx| {
                let random = ctx.random().clone();
                async move {
                    let latency = random.random_range(1..100u32);
                    // Quality watermark: re-fork on improvement (higher = better)
                    moonpool_sim::assert_sometimes_each!(
                        "latency_bucket",
                        [("type", 0i64)],
                        [("latency", latency as i64)]
                    );
                    Ok(())
                }
            })
            .set_iterations(5)
            .run()
            .await
    });

    assert_eq!(report.failed_runs, 0);
}
