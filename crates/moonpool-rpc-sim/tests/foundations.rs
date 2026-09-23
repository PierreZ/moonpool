//! The `sim-rpc-foundations` campaign as bounded, deterministic nextest
//! scenarios. The coverage-guided run is the `sim-rpc-foundations` binary
//! (`cargo xtask sim run rpc-foundations`).

use std::sync::{Arc, Mutex};

use moonpool_rpc_sim::foundations::{Observations, RunRecord, WorkloadConfig, campaign};
use moonpool_sim::{Chaos, ChaosMode, NetworkFault, NetworkFaultMask, SimulationReport};

/// A fixed seed budget, so every bounded scenario is deterministic. These
/// seeds are a budget, not witnesses: any change to the draw schedule may
/// move which of them hits a scenario, and the budget is sized with margin.
fn seed_budget(count: usize) -> Vec<u64> {
    (1..=count as u64).collect()
}

/// Seeds for the bit-flip scenario.
///
/// Bit flips need three independent per-seed buggify decisions to line up
/// (the rate knob's activation and firing, then the corruption site's
/// activation), about one seed in sixteen; eighty seeds leave margin.
const BIT_FLIP_SEEDS: usize = 80;

fn observations() -> Observations {
    Arc::new(Mutex::new(Vec::new()))
}

fn records(observations: &Observations) -> Vec<RunRecord> {
    observations
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

/// Every contract the campaign claims is observed within a fixed seed
/// budget, under the RNG canary, with no oracle violation.
#[test]
fn bounded_campaign_hits_every_required_scenario() {
    let observations = observations();
    let report = campaign(WorkloadConfig::campaign(), &observations)
        .check_determinism()
        .set_debug_seeds(seed_budget(12))
        .set_iterations(12)
        .run()
        .expect("simulation configuration is valid");
    report.eprint();
    assert_clean(&report);
    for required in [
        "rpc typed call succeeded",
        "rpc process-to-process call succeeded",
        "rpc malformed input closed the session",
        "rpc scripted corrupt frame closed the session",
        "rpc checksum mismatch on an established session",
        "rpc corruption failed an in-flight call ambiguously",
        "rpc unsupported protocol version refused",
        "rpc destroyed endpoint rejected a later call",
        "rpc contract mismatch rejected before decode",
        "rpc caller gave up after the request left",
        "rpc disconnect after server receipt is ambiguous",
        "rpc stale incarnation rejected after restart",
        "rpc server crashed after a handler received a request",
    ] {
        assert!(
            fired(&report, required),
            "required scenario never fired: {required}"
        );
    }
    assert!(!records(&observations).is_empty());
}

/// The same seed replays the same semantic history: every operation, its
/// request id and its outcome class, not just the same RNG draws.
#[test]
fn same_seed_replays_the_same_semantic_history() {
    let run = |seed| {
        let observations = observations();
        let report = campaign(WorkloadConfig::campaign(), &observations)
            .set_debug_seeds(vec![seed])
            .set_iterations(1)
            .run()
            .expect("simulation configuration is valid");
        assert_clean(&report);
        records(&observations)
    };
    for seed in [1, 7, 42] {
        let first = run(seed);
        assert_eq!(first.len(), 1);
        assert!(!first[0].history.is_empty());
        assert_eq!(first, run(seed), "seed {seed} diverged");
    }
}

/// Moonpool's own in-flight bit flips, and nothing else: corrupted frames
/// are caught by the checksum, the session is torn down, and no handler
/// ever sees a corrupted body (the handler asserts it on every request).
#[test]
fn network_bit_flips_are_caught_by_frame_checksums() {
    let observations = observations();
    let report = campaign(WorkloadConfig::traffic(), &observations)
        // Random keeps bit flips on every seed; the knob spikes their rate
        // on some seeds so a bounded budget meets corruption.
        .enable_chaos([Chaos::Network(ChaosMode::Random), Chaos::BuggifyKnobs])
        .network_fault_mask(NetworkFaultMask::none().with(NetworkFault::BitFlip))
        .set_debug_seeds(seed_budget(BIT_FLIP_SEEDS))
        .set_iterations(BIT_FLIP_SEEDS)
        .run()
        .expect("simulation configuration is valid");
    assert_clean(&report);
    let checksum_failures: u64 = records(&observations)
        .iter()
        .map(|record| record.established_checksum_failures)
        .sum();
    assert!(
        checksum_failures > 0,
        "no bit flip hit an RPC frame in the seed budget"
    );
    assert!(fired(
        &report,
        "rpc checksum mismatch on an established session"
    ));
}
