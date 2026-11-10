//! Binary target for the `sim-rpc-balance` campaign (#217).
//!
//! Balanced calls over explicit alternative sets under separate retry and
//! duplicate permissions, with outages of every alternative, destroyed
//! endpoints, cancellation and late losers, judged by the receipt and
//! attempt ledgers. Every seed runs twice under the RNG canary.

use std::process;

use moonpool_rpc_sim::balance::{BalanceCampaignConfig, balance_campaign};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    // No run records: the report is all the binary needs.
    let report = balance_campaign(BalanceCampaignConfig::campaign(), None)
        .check_determinism()
        .until_coverage_stable(10, 1000)
        .run()
        .expect("simulation configuration is valid");

    report.eprint();

    // Fails on a failing seed, a violation, a coverage gap and an
    // unsaturated sweep.
    process::exit(report.exit_code());
}
