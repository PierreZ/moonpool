//! Simulation workloads and binary targets.
//!
//! Contains workload definitions that can be used both in `#[test]` integration
//! tests and in standalone binary targets (where sancov instrumentation is
//! visible).

pub mod dungeon;
pub mod maze;

use crate::SimulationReport;

/// Run a SimulationBuilder inside a local tokio runtime and return the report.
pub fn run_simulation(builder: crate::SimulationBuilder) -> SimulationReport {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move { builder.run().await })
}
