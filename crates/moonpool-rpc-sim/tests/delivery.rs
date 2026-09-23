//! The `sim-rpc-delivery` campaign as bounded, deterministic nextest
//! scenarios. The coverage-guided run is the `sim-rpc-delivery` binary
//! (`cargo xtask sim run rpc-delivery`).

use std::sync::{Arc, Mutex};

use moonpool_rpc_sim::delivery::{
    DeliveryConfig, DeliveryRecord, DeliveryRecords, delivery_campaign,
};
use moonpool_sim::SimulationReport;

/// A fixed seed budget, so the bounded scenario is deterministic. These
/// seeds are a budget, not witnesses: a change to the draw schedule may
/// move which of them hits a scenario, and the budget is sized with margin
/// for the rarest required scenario.
const SEED_BUDGET: u64 = 40;

fn records() -> DeliveryRecords {
    Arc::new(Mutex::new(Vec::new()))
}

fn finished(records: &DeliveryRecords) -> Vec<DeliveryRecord> {
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

/// Every delivery contract the campaign claims is observed within a fixed
/// seed budget, every seed twice under the RNG canary, with no oracle
/// violation.
#[test]
fn bounded_campaign_hits_every_required_scenario() {
    let records = records();
    let report = delivery_campaign(DeliveryConfig::campaign(), &records)
        .check_determinism()
        .set_debug_seeds((1..=SEED_BUDGET).collect())
        .set_iterations(usize::try_from(SEED_BUDGET).unwrap_or(usize::MAX))
        .run()
        .expect("simulation configuration is valid");
    report.eprint();
    assert_clean(&report);
    let mut missing = Vec::new();
    for required in [
        // Delivery modes and what their outcomes prove.
        "rpc one-way request queued",
        "rpc timeout after sending is ambiguous",
        "rpc explicit no-reply left the caller to its deadline",
        "rpc broken promise reported to the caller",
        "rpc caller dropped a call in flight",
        "rpc late outcome collected by a kept attempt",
        "rpc late reply discarded after caller cleanup",
        // Lost replies: ambiguity for one attempt, duplicates for reliable.
        "rpc workload cut off from the server after execution",
        "rpc reply lost after execution is reported ambiguous",
        "rpc retained request retransmitted on a new connection",
        "rpc reliable request executed more than once",
        "rpc reply won a race with a disconnect",
        // Crashes: terminal dynamic failure, well-known recovery.
        "rpc delivery server crashed after a handler received a job",
        "rpc reliable call ended terminally at a stale incarnation",
        "rpc stale dynamic reference failed terminally",
        "rpc known-dead reference failed without a round trip",
        "rpc well-known endpoint answered a new incarnation",
        // Bootstrap names.
        "rpc bootstrap lookup failure stayed distinct",
        "rpc bootstrap re-resolved after a connection failure",
        // Peers and liveness.
        "rpc simultaneous connect resolved to one shared connection",
        "rpc accepted session adopted as the peer connection",
        "rpc ping timeout detected a dead connection",
        "rpc idle connection closed",
        "rpc reconnect waited out the backoff",
        "rpc peers with mixed sharing kept calling each other",
        "rpc peers cut apart with one-way reachability",
        "rpc peer reached through its own dial while ours stalled",
        "rpc peers agree on one shared connection at the end of the run",
        // Failure-monitor transitions.
        "rpc disconnect observed before the address failed",
        "rpc failure monitor marked the address failed",
        "rpc failed address recovered",
        "rpc reliable call bounded by sustained failure",
    ] {
        if !fired(&report, required) {
            missing.push(required);
        }
    }
    assert!(
        missing.is_empty(),
        "required scenarios never fired: {missing:?}"
    );
    let duplicates: u32 = finished(&records)
        .iter()
        .map(|record| record.duplicates)
        .sum();
    assert!(duplicates > 0, "no reliable request executed twice");
}

/// The same seed replays the same semantic history: every operation, its
/// job id and its outcome, not just the same RNG draws.
#[test]
fn same_seed_replays_the_same_semantic_history() {
    let run = |seed| {
        let records = records();
        let report = delivery_campaign(DeliveryConfig::campaign(), &records)
            .set_debug_seeds(vec![seed])
            .set_iterations(1)
            .run()
            .expect("simulation configuration is valid");
        assert_clean(&report);
        finished(&records)
    };
    for seed in [2, 9, 31] {
        let first = run(seed);
        assert_eq!(first.len(), 1);
        assert!(!first[0].history.is_empty());
        assert_eq!(first, run(seed), "seed {seed} diverged");
    }
}
