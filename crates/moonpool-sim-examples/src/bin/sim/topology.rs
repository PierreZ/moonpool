//! Binary target for the failure-domain topology simulation.
//!
//! Runs a 3-datacenter × 3-zone × 3-machine cluster (two processes per machine)
//! under machine-scoped attrition, so collocated processes reboot together.

use std::process;
use std::time::Duration;

use moonpool_sim::{
    Attrition, AttritionScope, Chaos, ChaosMode, LocalityConfig, SimulationBuilder,
};
use moonpool_sim_examples::topology::{PROCESSES_PER_MACHINE, TopologyProcess, TopologyWorkload};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = SimulationBuilder::new()
        .cluster(LocalityConfig::new(3, 3, 3, PROCESSES_PER_MACHINE), || {
            Box::new(TopologyProcess)
        })
        .workload(TopologyWorkload)
        .enable_chaos([Chaos::Attrition {
            config: Attrition {
                max_dead: PROCESSES_PER_MACHINE,
                prob_graceful: 0.3,
                prob_crash: 0.5,
                prob_wipe: 0.2,
                recovery_delay_ms: None,
                grace_period_ms: None,
                scope: AttritionScope::PerMachine,
            },
            mode: ChaosMode::Random,
        }])
        .chaos_duration(Duration::from_secs(10))
        .set_iterations(20)
        .run();

    report.eprint();

    if !report.seeds_failing.is_empty() {
        eprintln!(
            "ERROR: {} seeds failed: {:?}",
            report.seeds_failing.len(),
            report.seeds_failing
        );
        process::exit(1);
    }
}
