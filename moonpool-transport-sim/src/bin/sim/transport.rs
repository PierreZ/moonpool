//! Binary target for transport simulation.
//!
//! Runs the echo server process with transport client workloads under chaos,
//! exercising all 4 RPC delivery modes.

use std::process;
use std::time::Duration;

use moonpool_sim::SimulationBuilder;
use moonpool_sim::runner::builder::{ProcessCount, WorkloadCount};

use moonpool_transport_sim::process::EchoServerProcess;
use moonpool_transport_sim::workload::TransportClientWorkload;

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    let report = moonpool_sim::simulations::run_simulation(
        SimulationBuilder::new()
            .processes(ProcessCount::Range(1..=5), || Box::new(EchoServerProcess))
            .workloads(WorkloadCount::Random(1..10), |i| {
                Box::new(TransportClientWorkload::new(i))
            })
            .chaos_duration(Duration::from_secs(10))
            .random_network()
            .set_iterations(50),
    );

    report.eprint();

    if report.success_rate() < 1.0 {
        eprintln!(
            "FAILURE: success rate {:.1}%",
            report.success_rate() * 100.0
        );
        process::exit(1);
    }
}
