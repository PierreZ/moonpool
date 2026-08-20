//! Binary target for tonic gRPC simulation.
//!
//! Runs a tonic-based gRPC echo service over hyper HTTP/2 under deterministic
//! simulation, with network chaos plus Attrition (server crash/reboot).

use std::process;
use std::time::Duration;

use moonpool_sim::{Attrition, AttritionScope, Chaos, ChaosMode};

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
        .enable_chaos([Chaos::Attrition {
            config: Attrition {
                max_dead: 1,
                prob_graceful: 0.3,
                prob_crash: 0.5,
                prob_wipe: 0.2,
                recovery_delay_ms: None,
                grace_period_ms: None,
                scope: AttritionScope::PerProcess,
            },
            mode: ChaosMode::Random,
        }])
        .chaos_duration(Duration::from_secs(10))
        .set_iterations(50)
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
