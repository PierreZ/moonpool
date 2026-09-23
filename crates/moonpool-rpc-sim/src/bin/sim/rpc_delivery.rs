//! Binary target for the `sim-rpc-delivery` campaign (#214).
//!
//! Every delivery mode, lost replies, crashes, simultaneous connects,
//! bootstrap names and failure-monitor transitions under swarm network
//! chaos with every peer-policy knob buggified, judged by the execution
//! ledger. Every seed runs twice under the RNG canary.

use std::process;
use std::sync::{Arc, Mutex};

use moonpool_rpc_sim::delivery::{DeliveryConfig, delivery_campaign};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let records = Arc::new(Mutex::new(Vec::new()));
    let report = delivery_campaign(DeliveryConfig::campaign(), &records)
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
