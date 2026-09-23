//! Binary target for the `sim-rpc-interfaces` campaign (#215).
//!
//! Interfaces published, stored, recruited and forwarded across repeated
//! same-address restarts, judged by the boot, publication and execution
//! ledgers. Every seed runs twice under the RNG canary.

use std::process;
use std::sync::{Arc, Mutex};

use moonpool_rpc_sim::interfaces::{InterfacesConfig, interfaces_campaign};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let records = Arc::new(Mutex::new(Vec::new()));
    let report = interfaces_campaign(InterfacesConfig::campaign(), &records)
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
