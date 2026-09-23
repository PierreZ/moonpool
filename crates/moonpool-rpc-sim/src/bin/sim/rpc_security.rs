//! Binary target for the `sim-rpc-security` campaign (#218).
//!
//! Credential verification and endpoint access under key rotation, UTC
//! transitions, crashes, graceful shutdowns and mixed protocol versions,
//! judged by the issue/receipt ledger. Every seed runs twice under the RNG
//! canary.

use std::process;
use std::sync::{Arc, Mutex};

use moonpool_rpc_sim::security::{SecurityConfig, security_campaign};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let records = Arc::new(Mutex::new(Vec::new()));
    let report = security_campaign(SecurityConfig::campaign(), &records)
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
