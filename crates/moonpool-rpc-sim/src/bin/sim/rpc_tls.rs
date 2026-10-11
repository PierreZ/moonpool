//! Binary target for the `sim-rpc-tls` campaign.
//!
//! TLS 1.3 sessions, mutual or not, with seeded certificates, under
//! certificate rotation, UTC jumps, client-root changes, server crashes and
//! swarm network chaos with bit flips. Every seed runs twice under the RNG
//! canary.

use std::process;

use moonpool_rpc_sim::tls::tls_campaign;

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = tls_campaign()
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
