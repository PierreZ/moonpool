//! The run engine and the small system of the site (#416 (engine and player)).

use moonpool_wasm_demo::engine::{ChaosSet, RunConfig, run, run_json};

fn config(seed: u64, bug: bool) -> RunConfig {
    RunConfig {
        seed,
        bug,
        ..RunConfig::default()
    }
}

#[test]
fn one_seed_gives_the_same_run() {
    let a = run_json(r#"{"seed": 42}"#);
    assert_eq!(a, run_json(r#"{"seed": 42}"#));
    assert!(!a.contains("\"error\""), "{a}");
}

#[test]
fn a_run_records_messages_events_and_a_fingerprint() {
    let run = run(&config(7, false)).expect("valid config");
    assert_eq!(run.processes.len(), 4);
    assert!(run.events.iter().any(|e| e.name == "write_committed"));
    assert!(
        run.messages
            .iter()
            .any(|m| m.kind == "write" && m.received_ms.is_some())
    );
    assert!(
        run.messages
            .iter()
            .all(|m| m.received_ms.is_none_or(|r| r >= m.sent_ms))
    );
    assert!(run.events.windows(2).all(|w| w[0].seq < w[1].seq));
    assert_eq!(run.fingerprint.events.len(), 16);
    assert!(run.fingerprint.draws > 0);
    assert!(run.report.passed, "{:?}", run.report.violations);
}

#[test]
fn a_bad_config_is_an_error() {
    assert!(run_json(r#"{"scenario": "nope"}"#).contains("unknown scenario"));
    assert!(run_json(r#"{"servers": 9}"#).contains("1 to 5"));
    assert!(run_json(r#"{"colour": 1}"#).contains("bad config"));
}

#[test]
fn without_chaos_no_fault_is_injected() {
    let calm = RunConfig {
        chaos: ChaosSet {
            network: false,
            storage: false,
            reboots: false,
        },
        ..config(3, false)
    };
    let run = run(&calm).expect("valid config");
    assert!(run.faults.is_empty());
    // Each request arrives: only `buggify!` (a lost answer) drops a message.
    assert!(
        run.messages
            .iter()
            .filter(|m| m.id % 2 == 0)
            .all(|m| m.received_ms.is_some())
    );
}

/// The rates of the pull request: `cargo test -p moonpool-wasm-demo --test
/// engine -- --ignored --nocapture rates`.
#[test]
#[ignore = "measures the failure rate over many seeds"]
fn rates() {
    let n: u64 = std::env::var("SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    for bug in [true, false] {
        let failed = (1..=n)
            .filter(|&s| !run(&config(s, bug)).expect("valid").report.passed)
            .count();
        println!("bug={bug}: {failed} of {n} seeds fail");
    }
}
