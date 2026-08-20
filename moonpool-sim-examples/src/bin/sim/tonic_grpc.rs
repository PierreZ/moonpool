//! Binary target for tonic gRPC simulation.
//!
//! Runs a tonic-based gRPC echo service over hyper HTTP/2 under deterministic
//! simulation with chaos injection.

use std::process;

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = moonpool_sim::SimulationBuilder::new()
        .processes(1, || {
            Box::new(moonpool_sim_examples::tonic_grpc::EchoProcess)
        })
        .workload(moonpool_sim_examples::tonic_grpc::EchoWorkload)
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
