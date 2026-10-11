//! The `sim-rpc-tls` campaign as a bounded, deterministic nextest scenario.
//! The coverage-guided run is the `sim-rpc-tls` binary
//! (`cargo xtask sim run rpc-tls`).

use moonpool_rpc_sim::tls::tls_campaign;

/// A fixed seed budget (not witnesses): every seed twice under the RNG
/// canary.
const SEED_BUDGET: u64 = 32;

fn fired(report: &moonpool_sim::SimulationReport, message: &str) -> bool {
    report
        .assertion_results
        .get(message)
        .is_some_and(|stats| stats.successes > 0)
}

/// No oracle violation, and the contracts the campaign claims are observed.
#[test]
fn bounded_campaign_hits_every_required_scenario() {
    let report = tls_campaign()
        .check_determinism()
        .set_debug_seeds((1..=SEED_BUDGET).collect())
        .set_iterations(usize::try_from(SEED_BUDGET).unwrap_or(usize::MAX))
        .run()
        .expect("simulation configuration is valid");
    report.eprint();
    assert!(
        report.seeds_failing.is_empty(),
        "failing seeds {:?}: {:?}",
        report.seeds_failing,
        report.assertion_violations
    );
    assert!(
        report.assertion_violations.is_empty(),
        "{:?}",
        report.assertion_violations
    );
    let mut missing = Vec::new();
    for required in [
        "rpc tls member reached a private endpoint over mutual TLS",
        "rpc tls anonymous client reached a public endpoint",
        "rpc tls impostor server refused by its client",
        "rpc tls stranger refused by a private endpoint",
        "rpc tls member served after the faults",
        "rpc tls connect refused: certificate expired",
        "rpc tls connect refused: untrusted authority",
        "rpc tls server certificate rotated",
        "rpc tls member certificate rotated",
        "rpc tls server client roots moved away",
    ] {
        if !fired(&report, required) {
            missing.push(required);
        }
    }
    assert!(missing.is_empty(), "never observed: {missing:?}");
}
