//! Integration test for workload operation-alphabet swarm testing (issue #129).
//!
//! Verifies the end-to-end wiring: with `.swarm_operations()`, each seed enables a random
//! *subset* of a workload's operation alphabet (via [`swarm_op_enabled`]), so a
//! demonstrator that only triggers when a whole operation sub-group is masked
//! off becomes reachable across seeds — yet stays unreachable under the
//! always-on full alphabet (no swarm). This is the canonical swarm-testing win:
//! defeating passive/active suppression of bug-finding operations.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use moonpool_sim::{
    Process, SimContext, SimulationBuilder, SimulationReport, SimulationResult, Workload,
    swarm_op_enabled,
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
        .run()
        .expect("simulation configuration is valid");

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
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.failed_runs, 0, "baseline run had failed seeds");
    assert_eq!(
        group_suppressed_pass_count(&report),
        0,
        "without swarm the full alphabet is always on; the demonstrator is unreachable"
    );
}

/// A server process that only waits for its shutdown.
struct IdleProcess;

#[async_trait]
impl Process for IdleProcess {
    fn name(&self) -> &'static str {
        "server"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

/// One sample per seed: the process count of the group, and whether
/// operations 30 and 31 are on.
type Samples = Arc<Mutex<Vec<(usize, bool, bool)>>>;

/// Records the per-seed configuration that the run sees.
struct ShapeRecorder {
    samples: Samples,
}

#[async_trait]
impl Workload for ShapeRecorder {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let count = ctx.topology().ips_in_group("server").len();
        self.samples
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push((count, swarm_op_enabled(30), swarm_op_enabled(31)));
        Ok(())
    }
}

/// The process count and the operation swarm mask are independent across
/// seeds: every combination of a count in `1..=4` and the two operations on
/// or off occurs (#452). The configuration draws used to read the same
/// values as the first run draws, so operation 30 was on only for 2 or 4
/// processes and operation 31 only for 3 or 4.
#[test]
fn process_count_and_swarm_mask_are_independent() {
    let samples = Samples::default();
    let report = SimulationBuilder::new()
        .swarm_operations()
        .set_debug_seeds(seeds())
        .set_iterations(400)
        .processes(1..=4, || Box::new(IdleProcess))
        .workload(ShapeRecorder {
            samples: samples.clone(),
        })
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "report: {report:?}");

    let samples = samples.lock().expect("Mutex poisoned: prior task panicked");
    assert_eq!(samples.len(), 400);
    let seen: BTreeSet<(usize, bool, bool)> = samples.iter().copied().collect();
    for count in 1..=4 {
        for op30 in [false, true] {
            for op31 in [false, true] {
                assert!(
                    seen.contains(&(count, op30, op31)),
                    "no seed has {count} processes with op 30 = {op30} and op 31 = {op31}"
                );
            }
        }
    }
}
