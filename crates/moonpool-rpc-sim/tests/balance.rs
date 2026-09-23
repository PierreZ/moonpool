//! The `sim-rpc-balance` campaign as bounded, deterministic nextest
//! scenarios. The coverage-guided run is the `sim-rpc-balance` binary
//! (`cargo xtask sim run rpc-balance`).

use std::sync::{Arc, Mutex};

use moonpool_rpc_sim::balance::{
    BalanceCampaignConfig, BalanceRecord, BalanceRecords, balance_campaign,
};
use moonpool_sim::SimulationReport;

/// A fixed seed budget, so the bounded scenario is deterministic. These
/// seeds are a budget, not witnesses: a change to the draw schedule may
/// move which of them hits a scenario, and the budget is sized with margin
/// for the rarest required scenario.
const SEED_BUDGET: u64 = 30;

fn records() -> BalanceRecords {
    Arc::new(Mutex::new(Vec::new()))
}

fn finished(records: &BalanceRecords) -> Vec<BalanceRecord> {
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

/// Every balancing contract the campaign claims is observed within a
/// fixed seed budget, every seed twice under the RNG canary, with no
/// oracle violation.
#[test]
fn bounded_campaign_hits_every_required_scenario() {
    let records = records();
    let report = balance_campaign(BalanceCampaignConfig::campaign(), &records)
        .check_determinism()
        .set_debug_seeds((1..=SEED_BUDGET).collect())
        .set_iterations(usize::try_from(SEED_BUDGET).unwrap_or(usize::MAX))
        .run();
    report.eprint();
    assert_clean(&report);
    let mut missing = Vec::new();
    for required in [
        // All alternatives down, then restored.
        "rpc balance every alternative partitioned away",
        "rpc balance every server crashed",
        "rpc balance call failed while every alternative was down",
        "rpc balance call succeeded after every alternative was restored",
        "rpc balance stale set reported",
        "rpc balance all alternatives failed",
        // A destroyed endpoint next to healthy alternatives.
        "rpc balance server destroyed its endpoint and republished",
        "rpc balance destroyed endpoint skipped for a healthy one",
        // A stale incarnation (a restarted server) mixed with live ones.
        "rpc balance stale incarnation skipped for a healthy one",
        // Cancellation in the first and in the second attempt.
        "rpc balance call cancelled during its first attempt",
        "rpc balance call cancelled during its hedge",
        // Application-defined temporarily-behind classification.
        "rpc balance temporarily-behind reply failed over",
        "rpc balance exclusion applied",
        // Comparison hooks.
        "rpc balance comparison hook refused a divergent copy",
        "rpc balance comparison copy agreed with the winner",
        // Permissions, hedging and late losers.
        "rpc balance ambiguous attempt ended an at-most-once call",
        "rpc balance ambiguous attempt retried with permission",
        "rpc balance hedge won the race",
        "rpc balance hedge budget exhausted",
        "rpc balance loser still in flight when the winner answered",
        "rpc balance late loser updated the model after its call",
        "rpc balance late loser dropped at its bound",
    ] {
        if !fired(&report, required) {
            missing.push(required);
        }
    }
    assert!(
        missing.is_empty(),
        "required scenarios never fired: {missing:?}"
    );
    assert!(finished(&records).iter().all(|record| record.calls > 0));
}

/// The same seed replays the same semantic history: every job, its
/// operation and its outcome, not just the same RNG draws.
#[test]
fn same_seed_replays_the_same_semantic_history() {
    let run = |seed| {
        let records = records();
        let report = balance_campaign(BalanceCampaignConfig::campaign(), &records)
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
