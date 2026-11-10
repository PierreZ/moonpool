//! Assertion sites are declared at link time (#383): a site on a path that
//! no seed runs is reported once the run requires the declared sites of its
//! module.

#![cfg(not(target_family = "wasm"))]

use async_trait::async_trait;

use moonpool_sim::{
    SimContext, SimulationBuilder, SimulationResult, Workload, assert_always, assert_reachable,
    assert_sometimes, assert_sometimes_greater_than,
};

/// Reaches one path and never the other.
struct OnePathWorkload;

#[async_trait]
impl Workload for OnePathWorkload {
    fn name(&self) -> &'static str {
        "one_path"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        assert_reachable!("declared: the common path runs");
        assert_always!(true, "declared: the common path holds");
        // `my_ip()` is never empty, so this branch never runs.
        if ctx.my_ip().is_empty() {
            assert_always!(true, "declared: the dead path holds");
            assert_reachable!("declared: the dead path runs");
            assert_sometimes!(true, "declared: the dead path sometimes succeeds");
            assert_sometimes_greater_than!(1, 0, "declared: the dead path measures");
        }
        Ok(())
    }
}

fn run(builder: SimulationBuilder) -> moonpool_sim::SimulationReport {
    builder
        .workload(OnePathWorkload)
        .set_iterations(2)
        .set_debug_seeds(vec![1, 2])
        .run()
        .expect("simulation configuration is valid")
}

#[test]
fn the_registry_lists_literal_sites() {
    let sites = moonpool_sim::chaos::assertions::declared_sites();
    let here: Vec<_> = sites
        .iter()
        .filter(|site| site.module.starts_with("declared_sites"))
        .map(|site| site.msg)
        .collect();
    assert!(here.contains(&"declared: the dead path holds"), "{here:?}");
    assert!(here.contains(&"declared: the dead path runs"), "{here:?}");
}

#[test]
fn a_site_on_a_dead_path_is_never_reached() {
    let report = run(SimulationBuilder::new().require_declared_sites(["declared_sites"]));

    assert!(
        report
            .assertion_violations
            .iter()
            .any(|v| v.contains("declared: the dead path holds") && v.contains("never reached")),
        "{:?}",
        report.assertion_violations
    );
    for msg in [
        "declared: the dead path runs",
        "declared: the dead path sometimes succeeds",
        "declared: the dead path measures",
    ] {
        assert!(
            report
                .coverage_violations
                .iter()
                .any(|v| v.contains(msg) && v.contains("never reached")),
            "{msg}: {:?}",
            report.coverage_violations
        );
    }
    assert!(report.is_failure());
    // The sites that ran are not reported.
    assert!(
        !report
            .assertion_violations
            .iter()
            .chain(&report.coverage_violations)
            .any(|v| v.contains("common path"))
    );
}

#[test]
fn exempt_sites_and_other_modules_are_not_required() {
    let report = run(SimulationBuilder::new()
        .require_declared_sites(["declared_sites"])
        .exempt_declared_sites([
            "declared: the dead path holds",
            "declared: the dead path runs",
            "declared: the dead path sometimes succeeds",
            "declared: the dead path measures",
        ]));
    assert!(!report.is_failure(), "{report:?}");
    assert!(report.coverage_violations.is_empty());

    let report = run(SimulationBuilder::new().require_declared_sites(["some_other_crate"]));
    assert!(!report.is_failure(), "{report:?}");
}

#[test]
fn without_the_requirement_nothing_changes() {
    let report = run(SimulationBuilder::new());
    assert!(!report.is_failure(), "{report:?}");
    assert!(report.coverage_violations.is_empty());
}
