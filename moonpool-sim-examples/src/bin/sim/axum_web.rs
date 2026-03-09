//! Binary target for axum web service simulation.
//!
//! Runs an axum web service with fault-injectable in-memory store under
//! deterministic simulation with chaos injection.

use std::process;

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    let report = moonpool_sim::simulations::run_simulation(
        moonpool_sim::SimulationBuilder::new()
            .processes(1, || Box::new(moonpool_sim_examples::axum_web::WebProcess))
            .workload(moonpool_sim_examples::axum_web::WebWorkload)
            .set_iterations(10),
    );

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
