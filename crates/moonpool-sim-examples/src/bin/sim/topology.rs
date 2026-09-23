//! Binary target for the failure-domain topology simulation.
//!
//! Runs a 3-datacenter × 3-zone × 3-machine cluster (two processes per machine)
//! under machine-scoped attrition, so collocated processes reboot together.

use std::time::Duration;

use moonpool_sim::{AttritionScope, LocalityConfig, SimulationBuilder};
use moonpool_sim_examples::support::{finish_or_exit_on_failing_seeds, reboot_attrition};
use moonpool_sim_examples::topology::{PROCESSES_PER_MACHINE, TopologyProcess, TopologyWorkload};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = SimulationBuilder::new()
        .cluster(LocalityConfig::new(3, 3, 3, PROCESSES_PER_MACHINE), || {
            Box::new(TopologyProcess)
        })
        .workload(TopologyWorkload)
        .enable_chaos([reboot_attrition(
            PROCESSES_PER_MACHINE,
            AttritionScope::PerMachine,
        )])
        .chaos_duration(Duration::from_secs(10))
        .set_iterations(20)
        .run()
        .expect("simulation configuration is valid");

    finish_or_exit_on_failing_seeds(&report);
}
