//! The `sim-rpc-streams` campaign as bounded, deterministic nextest
//! scenarios. The coverage-guided run is the `sim-rpc-streams` binary
//! (`cargo xtask sim run rpc-streams`).

use std::sync::{Arc, Mutex};

use moonpool_rpc_sim::streams::{StreamsConfig, StreamsRecord, StreamsRecords, streams_campaign};
use moonpool_sim::SimulationReport;

/// A fixed seed budget, so the bounded scenario is deterministic. These
/// seeds are a budget, not witnesses: a change to the draw schedule may
/// move which of them hits a scenario, and the budget is sized with margin
/// for the rarest required scenario.
const SEED_BUDGET: u64 = 40;

fn records() -> StreamsRecords {
    Arc::new(Mutex::new(Vec::new()))
}

fn finished(records: &StreamsRecords) -> Vec<StreamsRecord> {
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

/// Every stream contract the campaign claims is observed within a fixed
/// seed budget, every seed twice under the RNG canary, with no oracle
/// violation.
#[test]
fn bounded_campaign_hits_every_required_scenario() {
    let records = records();
    let report = streams_campaign(StreamsConfig::campaign(), &records)
        .check_determinism()
        .set_debug_seeds((1..=SEED_BUDGET).collect())
        .set_iterations(usize::try_from(SEED_BUDGET).unwrap_or(usize::MAX))
        .run()
        .expect("simulation configuration is valid");
    report.eprint();
    assert_clean(&report);
    let mut missing = Vec::new();
    for required in [
        // Completion, credit and the two acknowledgement paths.
        "rpc stream completed normally with every item",
        "rpc stream producer waited for credit and resumed",
        "rpc stream item acknowledged on arrival by a waiting consumer",
        "rpc stream item acknowledged when popped from the queue",
        "rpc stream oversized item refused without waiting",
        // Abandonment and cancellation.
        "rpc stream abandoned before its first item",
        "rpc stream abandoned after its first item",
        "rpc stream caller timed out and abandoned the stream",
        "rpc stream producer observed cancellation",
        // Errors.
        "rpc stream error delivered under exhausted credit",
        "rpc stream broken promise reported after its items",
        // Saturation: control and unrelated work still progress.
        "rpc unary call progressed while streams saturated their credit",
        "rpc new stream progressed beside saturated ones",
        "rpc stream cancel took effect while the session was saturated",
        "rpc stream producer resumed after consumption",
        // Clog: the producer's writer is blocked behind a stalled reader.
        "rpc producer writer backed up behind a stalled reader",
        "rpc stream ack reached a producer with a blocked writer",
        "rpc stream cancel reached a producer with a blocked writer",
        "rpc unary call ran during a clog, replied after",
        "rpc streams resumed in order after a clog",
        // Admission budgets.
        "rpc request refused overloaded before admission",
        "rpc stream refused overloaded before admission",
        // Peer reboot and shutdown.
        "rpc stream producer crashed mid-stream",
        "rpc stream ended by a producer crash",
        "rpc stream producer shut down gracefully mid-stream",
        "rpc stream failed by a graceful producer shutdown",
    ] {
        if !fired(&report, required) {
            missing.push(required);
        }
    }
    assert!(
        missing.is_empty(),
        "required scenarios never fired: {missing:?}"
    );
    let runs = finished(&records);
    assert!(
        runs.iter()
            .all(|run| run.history.iter().any(|line| line == "recovered true")),
        "every run recovered unary and stream service after the faults"
    );
}

/// The same seed replays the same semantic history: every stream, what it
/// delivered and how it ended, not just the same RNG draws.
#[test]
fn same_seed_replays_the_same_semantic_history() {
    let run = |seed| {
        let records = records();
        let report = streams_campaign(StreamsConfig::campaign(), &records)
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
