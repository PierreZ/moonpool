//! Binary target for axum web service simulation.
//!
//! Runs an axum web service with fault-injectable in-memory store under
//! deterministic simulation with chaos injection.

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = moonpool_sim::SimulationBuilder::new()
        .processes(1, || Box::new(moonpool_sim_examples::axum_web::WebProcess))
        .workload(moonpool_sim_examples::axum_web::WebWorkload)
        .set_iterations(50)
        .run()
        .expect("simulation configuration is valid");

    moonpool_sim_examples::support::finish_or_exit_on_failure(&report);
}
