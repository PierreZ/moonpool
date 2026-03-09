//! Simulation workloads and binary targets.
//!
//! Contains workload definitions that can be used both in `#[test]` integration
//! tests and in standalone binary targets (where sancov instrumentation is
//! visible).

use crate::SimulationReport;

/// Run a SimulationBuilder and return the report.
pub fn run_simulation(builder: crate::SimulationBuilder) -> SimulationReport {
    builder.run()
}
