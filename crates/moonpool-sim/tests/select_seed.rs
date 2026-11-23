//! Every way into a simulation installs the seeded `select!` offset source,
//! and leaving the simulation removes it again.
//!
//! A `select!` inside a run that drew its branch offset from OS entropy
//! would make the run unreplayable from its seed, in silence. The ways in
//! are `SimulationBuilder::run`, `SimulationBuilder::live` and a raw
//! `SimWorld` (low-level tests). The runner also checks the source at the
//! start and the end of each run.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_core::select_support::select_offset_override_installed;
use moonpool_sim::{
    LiveStatus, Process, SimContext, SimWorld, SimulationBuilder, SimulationResult, TimeProvider,
    Workload,
};

/// A process that checks the source on each boot.
struct Node;

#[async_trait]
impl Process for Node {
    fn name(&self) -> &'static str {
        "node"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        assert!(select_offset_override_installed(), "process boot");
        while !ctx.shutdown().is_cancelled() {
            if ctx.time().sleep(Duration::from_millis(10)).await.is_err() {
                break;
            }
            assert!(select_offset_override_installed(), "process loop");
        }
        Ok(())
    }
}

/// A workload that checks the source in each phase.
struct Probe;

#[async_trait]
impl Workload for Probe {
    fn name(&self) -> &'static str {
        "probe"
    }

    async fn setup(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        assert!(select_offset_override_installed(), "workload setup");
        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        for _ in 0..5 {
            assert!(select_offset_override_installed(), "workload run");
            let _ = ctx.time().sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }

    async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        assert!(select_offset_override_installed(), "workload check");
        Ok(())
    }
}

fn builder() -> SimulationBuilder {
    SimulationBuilder::new()
        .processes(2, || Box::new(Node))
        .workload(Probe)
}

#[test]
fn run_installs_the_source_for_every_iteration_and_removes_it_after() {
    let report = builder().set_iterations(3).run().expect("valid builder");
    assert_eq!(report.failed_runs, 0, "{report:?}");
    assert!(
        !select_offset_override_installed(),
        "after the run, select! is back on the production source"
    );
}

#[test]
fn a_live_run_installs_the_source_and_removes_it_when_dropped() {
    let mut sim = builder().live().expect("a live run");
    assert!(select_offset_override_installed(), "a started live run");
    let status = sim.advance(&mut || false);
    assert!(matches!(status, LiveStatus::Finished(Ok(()))), "{status:?}");
    drop(sim);
    assert!(
        !select_offset_override_installed(),
        "after the live run, select! is back on the production source"
    );
}

#[test]
fn a_raw_world_installs_the_source_while_it_lives() {
    assert!(!select_offset_override_installed(), "no world yet");
    let first = SimWorld::new_with_seed(7);
    assert!(select_offset_override_installed(), "a live world");
    let second = SimWorld::new_with_seed(8);
    drop(first);
    assert!(
        select_offset_override_installed(),
        "one world dropped, another still lives"
    );
    drop(second);
    assert!(!select_offset_override_installed(), "every world dropped");
}
