//! The `sim-rpc-interfaces` campaign as bounded, deterministic nextest
//! scenarios. The coverage-guided run is the `sim-rpc-interfaces` binary
//! (`cargo xtask sim run rpc-interfaces`).

use std::sync::{Arc, Mutex};

use moonpool_rpc_sim::interfaces::{
    InterfacesConfig, InterfacesRecord, InterfacesRecords, interfaces_campaign,
};
use moonpool_sim::SimulationReport;

/// A fixed seed budget, so the bounded scenario is deterministic. These
/// seeds are a budget, not witnesses: a change to the draw schedule may
/// move which of them hits a scenario, and the budget is sized with margin
/// for the rarest required scenario.
const SEED_BUDGET: u64 = 30;

fn records() -> InterfacesRecords {
    Arc::new(Mutex::new(Vec::new()))
}

fn finished(records: &InterfacesRecords) -> Vec<InterfacesRecord> {
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

/// Every interface contract the campaign claims is observed within a fixed
/// seed budget, every seed twice under the RNG canary, with no oracle
/// violation.
#[test]
fn bounded_campaign_hits_every_required_scenario() {
    let records = records();
    let report = interfaces_campaign(InterfacesConfig::campaign(), &records)
        .check_determinism()
        .set_debug_seeds((1..=SEED_BUDGET).collect())
        .set_iterations(usize::try_from(SEED_BUDGET).unwrap_or(usize::MAX))
        .run()
        .expect("simulation configuration is valid");
    report.eprint();
    assert_clean(&report);
    let mut missing = Vec::new();
    for required in [
        // Same-address restarts, all three kinds.
        "rpc participant crashed and held down",
        "rpc participant rebooted in place",
        "rpc participant shut down gracefully",
        "rpc participants restarted repeatedly at the same address",
        "rpc participant delayed its registrations after a boot",
        "rpc directory lookup reached a boot before it registered",
        // Stale interfaces are refused, never redirected.
        "rpc stale interface refused after a same-address restart",
        "rpc stale interface failed fast after its first refusal",
        "rpc stale reliable call ended terminally",
        "rpc stored interface of an ended boot refused",
        // New incarnations are learned explicitly.
        "rpc new incarnation learned through republication",
        "rpc stale interface replaced only by an explicit lookup",
        "rpc call reached the incarnation that published its interface",
        // Recruitment and forwarding to a third participant.
        "rpc recruited interface invoked by a third participant",
        "rpc recruited interface refused after its participant restarted",
        "rpc dismissed recruited interface refused",
        "rpc foreign interface refused before any handler",
        "rpc ambiguous interface call executed in its published instance",
        "rpc every participant answered a fresh publication at the end",
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

/// The same seed replays the same semantic history: every operation, its
/// probe id and its outcome, not just the same RNG draws.
#[test]
fn same_seed_replays_the_same_semantic_history() {
    let run = |seed| {
        let records = records();
        let report = interfaces_campaign(InterfacesConfig::campaign(), &records)
            .set_debug_seeds(vec![seed])
            .set_iterations(1)
            .run()
            .expect("simulation configuration is valid");
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
