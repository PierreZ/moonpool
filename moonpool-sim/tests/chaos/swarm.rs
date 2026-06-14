//! End-to-end integration test for swarm testing (`SimulationBuilder::swarm`).
//!
//! Verifies that the swarm-subset wiring (per-seed `CONFIG_RNG` seeding plus the
//! swarm branch in `build_sim_for_iteration`) drives a full run cleanly across
//! many seeds: every seed builds its own per-seed fault subset and the
//! orchestrator completes. The per-seed subset logic itself (which families are
//! enabled, determinism, the all-off case) is unit-tested in
//! `moonpool-sim/src/network/config.rs`.
//!
//! The workload deliberately does not open connections. Under some swarm subsets
//! `ConnectFailureMode::Probabilistic` makes `connect()` hang forever — correct
//! chaos behavior, but it would make this plumbing test depend on which subsets
//! the entropy-seeded iteration happens to draw. `bind()` has no failure mode,
//! so it exercises the swarm-configured network provider without that coupling.

use async_trait::async_trait;
use moonpool_sim::{NetworkProvider, SimContext, SimulationBuilder, Workload};

/// Minimal workload: touches the swarm-configured network provider via `bind`
/// and fires a coverage gate. Always completes, regardless of fault subset.
struct SwarmSmokeWorkload;

#[async_trait]
impl Workload for SwarmSmokeWorkload {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> moonpool_sim::SimulationResult<()> {
        moonpool_sim::assert_sometimes!(true, "swarm run reached workload");
        // bind has no failure/hang mode, so this can't deadlock under any subset.
        if let Ok(listener) = ctx.network().bind("server").await {
            drop(listener);
        }
        Ok(())
    }
}

/// A swarm run over many seeds must build a per-seed subset and complete every
/// seed without deadlocks, panics, or orchestration failures.
#[test]
fn swarm_runs_clean_across_seeds() {
    let report = SimulationBuilder::new()
        .swarm()
        .set_iterations(50)
        .workload(SwarmSmokeWorkload)
        .run();

    assert_eq!(
        report.failed_runs, 0,
        "swarm run had {} failed seeds",
        report.failed_runs
    );
    assert_eq!(report.iterations, 50, "expected all 50 seeds to run");
}
