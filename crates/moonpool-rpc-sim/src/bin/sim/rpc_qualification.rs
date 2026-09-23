//! Binary target for the `sim-rpc-qualification` campaign (#219).
//!
//! Every RPC contract at once under combined faults, with a declared
//! recovery phase and resource baselines, judged by independent ledgers.
//! Every seed runs twice under the RNG canary.

use std::process;
use std::sync::{Arc, Mutex};

use moonpool_rpc_sim::qualification::{QualificationConfig, qualification_campaign};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let records = Arc::new(Mutex::new(Vec::new()));
    let report = qualification_campaign(QualificationConfig::campaign(), &records)
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
