//! `Live` against TLC on a small spec with a hidden bit: a stream longer
//! than one TLC behaviour, a hand-off of every state between chunks, a
//! rejection and a broken invariant counted from the first event of the
//! stream, and a run that sees the check stop.
//!
//! The tests need TLC (`tlc`, in the nix dev shell). Without it they are
//! skipped, except in CI, where they fail.

use std::path::PathBuf;

use moonpool_tla::{CheckError, Live, Trace, TraceCheck, Value, Verdict};

fn check() -> TraceCheck {
    TraceCheck::new(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/walker"),
        "WalkerTrace",
    )
}

/// Whether TLC can run here; a missing TLC fails only in CI.
fn tlc_or_skip() -> bool {
    if check().tlc_available() {
        return true;
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "TLC is missing in CI: run the tests in the nix dev shell"
    );
    eprintln!("skipped: TLC (`tlc`) is not installed");
    false
}

/// A live check in chunks of `chunk` events, with `limit` as the bound of
/// the counter.
fn live(chunk: usize, limit: i64) -> Live {
    let mut start = Trace::new();
    start.define("TraceLimit", Value::Int(limit));
    check()
        .live("TraceNext")
        .chunk(chunk)
        .start(&start)
        .expect("the spec folder keeps the contract")
}

fn none() -> Vec<(String, Value)> {
    Vec::new()
}

/// `count` incs from `from + 1`.
fn incs(live: &mut Live, from: i64, count: i64) {
    for x in from + 1..=from + count {
        live.event("inc", [("x", Value::Int(x))]);
    }
}

#[test]
fn a_stream_longer_than_one_behaviour_is_accepted() {
    if !tlc_or_skip() {
        return;
    }
    // TLC keeps one behaviour of at most 65,535 states: one run could not
    // follow these 70,000 events.
    let mut live = live(10_000, 100_000);
    incs(&mut live, 0, 70_000);
    assert_eq!(live.events(), 70_000);
    assert_eq!(live.finish().expect("verdict"), Verdict::Accepted);
}

#[test]
fn every_state_at_the_end_of_a_chunk_goes_on() {
    if !tlc_or_skip() {
        return;
    }
    // The bit is set before the first chunk ends and shown four chunks
    // later: both values must cross every hand-off.
    for shown in [0, 1] {
        let mut live = live(25, 1000);
        live.event("flip", none());
        incs(&mut live, 0, 100);
        live.event("show", [("h", Value::Int(shown))]);
        assert_eq!(
            live.finish().expect("verdict"),
            Verdict::Accepted,
            "{shown}"
        );
    }
}

#[test]
fn a_rejection_counts_from_the_first_event() {
    if !tlc_or_skip() {
        return;
    }
    let mut live = live(25, 1000);
    live.event("flip", none());
    incs(&mut live, 0, 60);
    live.event("show", [("h", Value::Int(2))]);
    let Verdict::Rejected(rejection) = live.finish().expect("verdict") else {
        panic!("the bit is never 2");
    };
    assert_eq!(rejection.index, 61);
    // The states before the event: the two values of the hidden bit.
    assert!(rejection.state.contains("h |-> 0"), "{}", rejection.state);
    assert!(rejection.state.contains("h |-> 1"), "{}", rejection.state);
}

#[test]
fn a_rejection_at_the_start_of_a_chunk_shows_the_hand_off() {
    if !tlc_or_skip() {
        return;
    }
    let mut live = live(10, 1000);
    incs(&mut live, 0, 10);
    live.event("inc", [("x", Value::Int(12))]);
    let Verdict::Rejected(rejection) = live.finish().expect("verdict") else {
        panic!("a jump from 10 to 12 is not a step");
    };
    assert_eq!(rejection.index, 10);
    assert!(rejection.state.contains("x |-> 10"), "{}", rejection.state);
}

#[test]
fn a_broken_invariant_counts_from_the_first_event() {
    if !tlc_or_skip() {
        return;
    }
    let mut live = live(20, 50);
    incs(&mut live, 0, 60);
    let verdict = live.finish().expect("verdict");
    let Verdict::InvariantViolated {
        invariant, matched, ..
    } = verdict
    else {
        panic!("x = 50 breaks Small: {verdict:?}");
    };
    assert_eq!(invariant, "Small");
    assert_eq!(matched, 50);
}

#[test]
fn the_run_sees_the_check_stop() {
    if !tlc_or_skip() {
        return;
    }
    let mut live = live(5, 1000);
    live.event("inc", [("x", Value::Int(2))]);
    let mut x = 2;
    // The run goes on until the check tells it to stop.
    while live.event("inc", [("x", Value::Int(x + 1))]) {
        x += 1;
        assert!(x < 1_000_000, "the check never stopped the run");
        std::thread::yield_now();
    }
    assert!(live.is_stopped());
    let Verdict::Rejected(rejection) = live.finish().expect("verdict") else {
        panic!("the first event is not a step");
    };
    assert_eq!(rejection.index, 0);
}

#[test]
fn a_spec_without_the_cursor_is_refused() {
    if !tlc_or_skip() {
        return;
    }
    let mut start = Trace::new();
    start.define("TraceLimit", Value::Int(10));
    let mut live = check()
        .cursor("nope")
        .live("TraceNext")
        .start(&start)
        .expect("started");
    live.event("inc", [("x", Value::Int(1))]);
    let error = live.finish().expect_err("the spec has no variable nope");
    assert!(
        matches!(error, CheckError::Tlc { .. } | CheckError::Contract(_)),
        "{error}"
    );
}
