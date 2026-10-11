//! Binary target for the operator simulation.
//!
//! A workload racks and decommissions nodes through `ctx.deployment()` while
//! a client pings them, under process attrition.

use std::time::Duration;

use moonpool_sim::{AttritionScope, LocalityConfig, SimulationBuilder};
use moonpool_sim_examples::operator::{OperatorWorkload, PingWorkload, RackNode};
use moonpool_sim_examples::support::{finish_or_exit_on_failure, reboot_attrition};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = SimulationBuilder::new()
        .cluster(LocalityConfig::new(1, 2, 2, 1), || Box::new(RackNode))
        .workload(OperatorWorkload)
        .workload(PingWorkload)
        .enable_chaos([reboot_attrition(1, AttritionScope::PerProcess)])
        .chaos_duration(Duration::from_secs(15))
        .max_spawns(8)
        .set_iterations(20)
        .run()
        .expect("simulation configuration is valid");

    finish_or_exit_on_failure(&report);
}
