//! Binary target for metastable failure exploration.
//!
//! Runs fork-based exploration to discover the metastable failure state where
//! DNS heals but the system stays broken due to thundering herd.

use std::process;

use moonpool::simulations::metastable::{
    dns::DnsWorkload, driver::DriverWorkload, fleet::FleetWorkload, invariants::RecoveryInvariant,
    lease_store::LeaseStoreWorkload, run_simulation,
};
use moonpool_sim::{ExplorationConfig, SimulationBuilder};

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .workload(DnsWorkload::new())
            .workload(LeaseStoreWorkload::new())
            .workload(FleetWorkload::new())
            .workload(DriverWorkload::new())
            .invariant(RecoveryInvariant::new())
            .random_network()
            .set_iterations(2)
            .set_debug_seeds(vec![54321])
            .enable_exploration(ExplorationConfig {
                max_depth: 60,
                timelines_per_split: 4,
                global_energy: 20_000,
                adaptive: Some(moonpool_sim::AdaptiveConfig {
                    batch_size: 20,
                    min_timelines: 60,
                    max_timelines: 200,
                    per_mark_energy: 2_000,
                    warm_min_timelines: Some(20),
                }),
                parallelism: Some(moonpool_sim::Parallelism::HalfCores),
            }),
    );

    report.eprint();

    let metastable = report.assertion_results.get("metastable_failure_detected");
    if let Some(m) = metastable
        && m.successes > 0
    {
        eprintln!("Metastable failure detected!");
        process::exit(0);
    }

    process::exit(0);
}
