//! The `sim-rpc-qualification` campaign as bounded, deterministic nextest
//! scenarios. The coverage-guided run is the `sim-rpc-qualification`
//! binary (`cargo xtask sim run rpc-qualification`).
//!
//! Reproduce one seed: `qualification_campaign(QualificationConfig::campaign(), &records)
//! .set_debug_seeds(vec![seed]).set_iterations(1).run()?`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_rpc_sim::qualification::{
    HostJitter, QualificationConfig, QualificationRecord, QualificationRecords,
    qualification_campaign,
};
use moonpool_sim::SimulationReport;

/// A fixed seed budget, so the bounded scenario is deterministic. These
/// seeds are a budget, not witnesses: a change to the draw schedule may
/// move which of them hits a scenario, and the budget is sized with margin
/// for the rarest required scenario.
const SEED_BUDGET: u64 = 30;

/// Where the seed budget starts: `QUAL_SEED_OFFSET=n` runs seeds
/// `n + 1..=n + SEED_BUDGET` instead of `1..=SEED_BUDGET`, to check that the
/// budget holds its margin on other windows (a local check; CI runs 0).
fn offset() -> u64 {
    std::env::var("QUAL_SEED_OFFSET")
        .ok()
        .and_then(|offset| offset.parse().ok())
        .unwrap_or(0)
}

fn records() -> QualificationRecords {
    Arc::new(Mutex::new(Vec::new()))
}

fn finished(records: &QualificationRecords) -> Vec<QualificationRecord> {
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

/// Every scenario the qualification claims, combined in one campaign: a
/// green coverage plateau alone cannot satisfy an unobserved one.
const REQUIRED: &[&str] = &[
    // Reliable ambiguity and duplicates.
    "rpc qual reply lost after execution reported ambiguous",
    "rpc qual timeout after execution",
    "rpc qual caller dropped a call in flight",
    "rpc qual reliable request executed more than once",
    "rpc qual reliable duplicate while a stream shared the session",
    "rpc qual reliable call bounded by sustained failure",
    "rpc qual reply won a race with its server going down",
    // Paros publication and same-address reboots.
    "rpc qual participant republished after a same-address reboot",
    "rpc qual stale I1 refused while I2 served at the same address",
    "rpc qual retained reliable call to I1 ended terminally",
    "rpc qual lookup reached a boot before it registered",
    "rpc qual server delayed its registrations after a boot",
    "rpc qual stored interface of an ended boot refused",
    "rpc qual recruited interface invoked by the third participant",
    "rpc qual callback invoked through a third participant",
    "rpc qual stale callback refused",
    // Streams.
    "rpc qual stream abandoned before its first item",
    "rpc qual stream abandoned after its first item",
    "rpc qual slow consumer held the producer to its window",
    "rpc qual producer exhausted credit and resumed",
    "rpc qual stream ended by a disconnect mid-stream",
    "rpc qual stream failed by a graceful shutdown",
    // Balancing.
    "rpc qual balanced call fell back to another alternative",
    "rpc qual balanced call hedged",
    "rpc qual hedge budget exhausted",
    "rpc qual late loser completed after its call returned",
    "rpc qual stale incarnation skipped for a healthy one",
    "rpc qual credentialed balanced call served",
    // Credentials and versions.
    "rpc qual expired credential denied before dispatch",
    "rpc qual rotated key denied then new key accepted",
    "rpc qual private endpoint refused an unauthenticated call",
    "rpc qual local call denied by the same policy",
    "rpc qual a denial was audited in the trace",
    "rpc qual adjacent-version peer interoperated",
    "rpc qual unsupported version refused observably",
    "rpc qual malformed peer closed, other sessions unaffected",
    // Lifecycle and overload.
    "rpc qual server crashed and held down",
    "rpc qual server rebooted in place",
    "rpc qual server shut down gracefully under load",
    "rpc qual server shut down gracefully on request",
    "rpc qual graceful shutdown drained an in-flight call",
    "rpc qual graceful shutdown ended work at its deadline",
    "rpc qual shutdown refused new admissions",
    "rpc qual third participant restarted",
    "rpc qual client partitioned from every server",
    "rpc qual lane cut off from a server holding its reliable call",
    "rpc qual key set rotated",
    "rpc qual UTC stepped past token lifetimes",
    "rpc qual request refused overloaded before admission",
    "rpc qual service recovered after an overload burst",
    // Recovery.
    "rpc qual every service recovered after the faults",
];

/// Every required scenario is observed within a fixed seed budget, every
/// seed twice under the RNG canary, with no oracle violation; every run
/// recovered within the declared bound and ended at its resource baseline.
#[test]
fn bounded_campaign_hits_every_required_scenario() {
    let records = records();
    let report = qualification_campaign(QualificationConfig::campaign(), &records)
        .check_determinism()
        .set_debug_seeds((offset() + 1..=offset() + SEED_BUDGET).collect())
        .set_iterations(usize::try_from(SEED_BUDGET).unwrap_or(usize::MAX))
        .run()
        .expect("simulation configuration is valid");
    report.eprint();
    assert_clean(&report);
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|required| !fired(&report, required))
        .collect();
    assert!(
        missing.is_empty(),
        "required scenarios never fired: {missing:?}"
    );
    let finished = finished(&records);
    // One record per run: every seed twice under the canary.
    assert_eq!(
        finished.len(),
        2 * usize::try_from(SEED_BUDGET).unwrap_or(usize::MAX)
    );
    for record in &finished {
        assert!(!record.history.is_empty());
        assert!(record.recovered_ms.is_some(), "a run did not recover");
        assert!(record.baseline, "a run ended away from its baseline");
    }
}

/// The same seed replays the same semantic record: history, ledger digest
/// and trace, not just the same RNG draws.
#[test]
fn same_seed_replays_the_same_semantic_history() {
    for seed in [3, 11, 27] {
        let first = run(seed, QualificationConfig::campaign());
        assert!(!first.history.is_empty() && !first.trace.is_empty());
        assert_eq!(
            first,
            run(seed, QualificationConfig::campaign()),
            "seed {seed} diverged"
        );
    }
}

fn run(seed: u64, config: QualificationConfig) -> QualificationRecord {
    let records = records();
    let report = qualification_campaign(config, &records)
        .set_debug_seeds(vec![seed])
        .set_iterations(1)
        .run()
        .expect("simulation configuration is valid");
    assert_clean(&report);
    let mut finished = finished(&records);
    assert_eq!(finished.len(), 1);
    finished.remove(0)
}

/// Complete simulations one after another in one process: a run leaves
/// nothing behind that changes the next (same seed, same record), and
/// every run ends at its resource baseline.
#[test]
fn repeated_complete_runs_are_isolated() {
    let mut seen: Vec<(u64, QualificationRecord)> = Vec::new();
    for seed in [5, 9, 5, 13, 5] {
        let record = run(seed, QualificationConfig::campaign());
        assert!(record.baseline, "seed {seed} ended away from its baseline");
        if let Some((_, earlier)) = seen.iter().find(|(earlier, _)| *earlier == seed) {
            assert_eq!(earlier, &record, "seed {seed} changed after other runs");
        }
        seen.push((seed, record));
    }
}

/// One server restarted in place at least ten times while the workload
/// keeps using it: every boot finds the previous runtime at its baseline,
/// and service recovers afterwards.
#[test]
fn reboot_storm_returns_to_baseline() {
    let records = records();
    let report = qualification_campaign(QualificationConfig::reboot_storm(), &records)
        .set_debug_seeds(vec![1, 2, 3])
        .set_iterations(3)
        .run()
        .expect("simulation configuration is valid");
    assert_clean(&report);
    assert!(fired(&report, "rpc qual one server rebooted ten times"));
    assert!(fired(
        &report,
        "rpc qual a tenth reboot still found its runtimes at baseline"
    ));
    for record in finished(&records) {
        assert!(record.recovered_ms.is_some() && record.baseline);
    }
}

/// Host speed changes nothing: the same seed with every poll slowed (a
/// busy spin, or the thread put to sleep every few polls) replays the
/// same record. No decision may come from wall-clock time.
#[test]
fn host_speed_does_not_change_semantics() {
    let seed = 7;
    let baseline = run(seed, QualificationConfig::campaign());
    for jitter in [
        HostJitter::Spin(2_000),
        HostJitter::Stall {
            every: 64,
            sleep: Duration::from_micros(200),
        },
    ] {
        let jittered = run(seed, QualificationConfig::campaign().with_jitter(jitter));
        assert_eq!(baseline, jittered, "{jitter:?} changed the run");
    }
}
