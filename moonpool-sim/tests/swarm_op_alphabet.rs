//! Integration test for workload operation-alphabet swarm testing (issue #129).
//!
//! Verifies the end-to-end wiring: with `.swarm_operations()`, each seed enables a random
//! *subset* of a workload's operation alphabet (via [`swarm_op_enabled`]), so a
//! demonstrator that only triggers when a whole operation sub-group is masked
//! off becomes reachable across seeds — yet stays unreachable under the
//! always-on full alphabet (no swarm). This is the canonical swarm-testing win:
//! defeating passive/active suppression of bug-finding operations.

use async_trait::async_trait;
use moonpool_sim::{
    SimContext, SimulationBuilder, SimulationReport, SimulationResult, Workload, swarm_op_enabled,
};

/// The demonstrator fires when none of the first four operations is enabled.
const GROUP: u8 = 4;

/// Minimal workload over an operation alphabet. Mirrors the dungeon reference
/// pattern: emit the demonstrator assertion only on the path where it holds, so
/// it never accrues a 0%-success coverage violation when unreachable.
struct OpAlphabetWorkload;

#[async_trait]
impl Workload for OpAlphabetWorkload {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        // Always reached: proves the workload ran this seed.
        moonpool_sim::assert_sometimes!(true, "op alphabet workload reached");

        // Reachable only when the whole leading operation group is masked off
        // (P = (1/2)^GROUP per seed under swarm); impossible when the full
        // alphabet is always on.
        let group_suppressed = !(0..GROUP).any(swarm_op_enabled);
        if group_suppressed {
            moonpool_sim::assert_sometimes!(true, "operation group suppressed by swarm");
        }

        Ok(())
    }
}

fn group_suppressed_pass_count(report: &SimulationReport) -> u64 {
    report
        .assertion_details
        .iter()
        .find(|d| d.msg == "operation group suppressed by swarm")
        .map_or(0, |d| d.pass_count)
}

/// 400 explicit seeds make the run fully deterministic (independent of the
/// wall-clock base seed). With `GROUP == 4` the demonstrator fires ~6% of
/// seeds, so it is hit many times within 400.
fn seeds() -> Vec<u64> {
    (0..400u64).collect()
}

#[test]
fn swarm_makes_group_suppression_reachable() {
    let report = SimulationBuilder::new()
        .swarm_operations()
        .set_debug_seeds(seeds())
        .set_iterations(400)
        .workload(OpAlphabetWorkload)
        .run();

    assert_eq!(report.failed_runs, 0, "swarm run had failed seeds");
    assert!(
        group_suppressed_pass_count(&report) > 0,
        "the suppressed-group demonstrator must be reachable under swarm"
    );
}

#[test]
fn full_alphabet_never_suppresses_group() {
    let report = SimulationBuilder::new()
        .set_debug_seeds(seeds())
        .set_iterations(400)
        .workload(OpAlphabetWorkload)
        .run();

    assert_eq!(report.failed_runs, 0, "baseline run had failed seeds");
    assert_eq!(
        group_suppressed_pass_count(&report),
        0,
        "without swarm the full alphabet is always on; the demonstrator is unreachable"
    );
}
