//! Binary target for the `sim-rpc-streams` campaign (#216).
//!
//! Reply streams with consumption-based credit under swarm network chaos,
//! squeezed admission and stream budgets, abandonment, saturation and
//! producer reboots, judged by the producer and consumer ledgers. Every
//! seed runs twice under the RNG canary.

use std::process;
use std::sync::{Arc, Mutex};

use moonpool_rpc_sim::streams::{StreamsConfig, streams_campaign};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let records = Arc::new(Mutex::new(Vec::new()));
    let report = streams_campaign(StreamsConfig::campaign(), &records)
        .check_determinism()
        .until_coverage_stable(10, 1000)
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
