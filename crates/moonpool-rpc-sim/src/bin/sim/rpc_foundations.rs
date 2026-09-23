//! Binary target for the `sim-rpc-foundations` campaign.
//!
//! Typed dynamic-endpoint RPC between a server and a relay process group and
//! a surviving workload, under swarm network chaos (bit flips included) and
//! scripted crash-after-receipt restarts, judged by the handler receipt
//! ledger. Every seed runs twice under the RNG canary.

use std::process;
use std::sync::{Arc, Mutex};

use moonpool_rpc_sim::foundations::{WorkloadConfig, campaign};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let observations = Arc::new(Mutex::new(Vec::new()));
    let report = campaign(WorkloadConfig::campaign(), &observations)
        .check_determinism()
        .until_coverage_stable(10, 1000)
        .run()
        .expect("simulation configuration is valid");

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
