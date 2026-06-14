//! Binary target for the transport simulation: hash-chain workload.
//!
//! 1 server + 1 workload + integrity invariant under transport chaos.
//! Follow-ups: enable attrition + storage-backed persistence for crash recovery.

use std::process;
use std::time::Duration;

use moonpool_sim::SimulationBuilder;
use moonpool_sim::runner::builder::{ProcessCount, WorkloadCount};

use moonpool_transport_sim::invariants::TransportIntegrityInvariant;
use moonpool_transport_sim::process::TransportServerProcess;
use moonpool_transport_sim::workload::TransportClientWorkload;

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::INFO);

    let report = SimulationBuilder::new()
        .processes(ProcessCount::Fixed(1), || Box::new(TransportServerProcess))
        .workloads(WorkloadCount::Fixed(1), |i| {
            Box::new(TransportClientWorkload::new(i))
        })
        .invariant(TransportIntegrityInvariant::new())
        .chaos_duration(Duration::from_secs(10))
        // Swarm: each seed runs a random *subset* of both the network fault
        // families and the workload's operation alphabet (the rest fully off),
        // defeating passive/active suppression. Supersedes `.random_network()`.
        .swarm()
        .set_iterations(20)
        .seed_warning_timeout(Duration::from_secs(15))
        .run();

    report.eprint();

    if report.success_rate() < 1.0 {
        eprintln!(
            "FAILURE: success rate {:.1}%",
            report.success_rate() * 100.0
        );
        process::exit(1);
    }
}
