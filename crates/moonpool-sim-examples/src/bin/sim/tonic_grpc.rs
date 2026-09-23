//! Binary target for tonic gRPC simulation.
//!
//! Runs a tonic-based gRPC echo service over hyper HTTP/2 under deterministic
//! simulation, with network chaos plus Attrition (server crash/reboot).

use std::time::Duration;

use moonpool_sim::AttritionScope;
use moonpool_sim_examples::support::{finish_or_exit_on_failure, reboot_attrition};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = moonpool_sim::SimulationBuilder::new()
        .processes(1, || {
            Box::new(moonpool_sim_examples::tonic_grpc::EchoProcess)
        })
        .workload(moonpool_sim_examples::tonic_grpc::EchoWorkload)
        // On top of the default network chaos, kill and restart the gRPC
        // server while the workload runs: rounds must survive dead servers,
        // reconnects, and fresh process state.
        .enable_chaos([reboot_attrition(1, AttritionScope::PerProcess)])
        .chaos_duration(Duration::from_secs(10))
        .set_iterations(50)
        .run()
        .expect("simulation configuration is valid");

    finish_or_exit_on_failure(&report);
}
