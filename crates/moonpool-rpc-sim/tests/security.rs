//! The `sim-rpc-security` campaign as bounded, deterministic nextest
//! scenarios. The coverage-guided run is the `sim-rpc-security` binary
//! (`cargo xtask sim run rpc-security`).

use std::sync::{Arc, Mutex};

use moonpool_rpc_sim::security::{
    SecurityConfig, SecurityRecord, SecurityRecords, security_campaign,
};
use moonpool_sim::SimulationReport;

/// A fixed seed budget, so the bounded scenario is deterministic. These
/// seeds are a budget, not witnesses: a change to the draw schedule may
/// move which of them hits a scenario, and the budget is sized with margin
/// for the rarest required scenario.
const SEED_BUDGET: u64 = 24;

fn records() -> SecurityRecords {
    Arc::new(Mutex::new(Vec::new()))
}

fn finished(records: &SecurityRecords) -> Vec<SecurityRecord> {
    records
        .lock()
        .expect("Mutex poisoned: prior task panicked")
        .clone()
}

fn assert_clean(report: &SimulationReport) {
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
}

fn fired(report: &SimulationReport, message: &str) -> bool {
    report
        .assertion_results
        .get(message)
        .is_some_and(|stats| stats.successes > 0)
}

/// Every security contract the campaign claims is observed within a fixed
/// seed budget, every seed twice under the RNG canary, with no oracle
/// violation.
#[test]
fn bounded_campaign_hits_every_required_scenario() {
    let records = records();
    let report = security_campaign(SecurityConfig::campaign(), &records)
        .check_determinism()
        .set_debug_seeds((1..=SEED_BUDGET).collect())
        .set_iterations(usize::try_from(SEED_BUDGET).unwrap_or(usize::MAX))
        .run();
    report.eprint();
    assert_clean(&report);
    let mut missing = Vec::new();
    for required in [
        // Accepted, on every route.
        "rpc security verified token reached a private endpoint",
        "rpc security verified token reached a private endpoint locally",
        "rpc security anonymous caller reached a public endpoint",
        "rpc security authorized stream delivered its items",
        // Refused before dispatch, for what is wrong with the credential.
        "rpc security anonymous call refused by a private endpoint",
        "rpc security forged signature refused",
        "rpc security unknown key refused",
        "rpc security retired key refused",
        "rpc security wrong audience refused",
        "rpc security wrong issuer refused",
        "rpc security symmetric algorithm confusion refused",
        "rpc security malformed credential refused",
        "rpc security expired token refused",
        "rpc security not-yet-valid token refused",
        "rpc security token refused while the server had no UTC",
        "rpc security invalid credential refused on a public endpoint",
        "rpc security local call refused like a remote one",
        "rpc security unauthorized stream refused before any item",
        "rpc security invalid one-way request sent",
        // Key and time transitions.
        "rpc security UTC jumped forward",
        "rpc security key set rotated",
        "rpc security servers lost the UTC time",
        "rpc security the same token accepted, then refused",
        "rpc security workload cut off from the server",
        "rpc security reliable call resent with a fresh credential",
        "rpc security resent call refused after an earlier copy left",
        // Versions.
        "rpc security v1 client refused at the handshake",
        "rpc security legacy v1 server answered a public call",
        "rpc security v1 session kept out of a private endpoint",
        // Lifecycle.
        "rpc security server crashed mid-work",
        "rpc security server shut down gracefully mid-work",
        "rpc security graceful shutdown drained its work",
        "rpc security graceful shutdown ended work left at its deadline",
        "rpc security request refused while the server drained",
        // Recovery.
        "rpc security valid token served after the faults",
    ] {
        if !fired(&report, required) {
            missing.push(required);
        }
    }
    assert!(
        missing.is_empty(),
        "required scenarios never fired: {missing:?}"
    );
    assert!(
        finished(&records)
            .iter()
            .all(|record| !record.history.is_empty())
    );
}

/// The same seed replays the same semantic history: every request, its
/// credential kind and its outcome, not just the same RNG draws.
#[test]
fn same_seed_replays_the_same_semantic_history() {
    let run = |seed| {
        let records = records();
        let report = security_campaign(SecurityConfig::campaign(), &records)
            .set_debug_seeds(vec![seed])
            .set_iterations(1)
            .run();
        assert_clean(&report);
        finished(&records)
    };
    for seed in [3, 11, 27] {
        let first = run(seed);
        assert_eq!(first.len(), 1);
        assert!(!first[0].history.is_empty());
        assert_eq!(first, run(seed), "seed {seed} diverged");
    }
}
