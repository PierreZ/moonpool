//! Binary target for the metered key/value simulation.
//!
//! Three nodes, each with its own `prometheus::Registry`, running under
//! network chaos. The report's Application Metrics section shows what they
//! recorded, folded across every successful seed.

use std::sync::Arc;
use std::time::Duration;

use moonpool_prometheus::PrometheusSource;
use moonpool_sim::{Chaos, ChaosMode, SimulationBuilder};
use moonpool_sim_examples::metrics_service::{MeteredKvNode, MeteredKvWorkload};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = SimulationBuilder::new()
        // One registry per node: every simulated node shares this OS process,
        // so a single shared registry would merge their counters into one.
        .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
        .processes(3, || Box::new(MeteredKvNode::new()))
        .workload(MeteredKvWorkload)
        .enable_chaos([Chaos::Network(ChaosMode::Swarm)])
        .chaos_duration(Duration::from_secs(3))
        .set_iterations(10)
        .run()
        .expect("simulation configuration is valid");

    moonpool_sim_examples::support::finish_or_exit_on_failure(&report);
}
