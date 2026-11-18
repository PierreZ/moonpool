//! `TraceCheck` against TLC on a small counter spec: an accepted trace, a
//! rejected one with the first event that does not match, a broken
//! invariant, and a spec TLC cannot read.
//!
//! The tests need TLC (`tlc`, in the nix dev shell). Without it they are
//! skipped, except in CI, where they fail.

use std::path::PathBuf;

use moonpool_tla::{CheckError, Trace, TraceCheck, Value, Verdict};

fn spec_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/counter")
}

fn check() -> TraceCheck {
    TraceCheck::new(spec_dir(), "CounterTrace")
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

/// One `inc` event per value, in order.
fn incs(values: &[i64]) -> Trace {
    let mut trace = Trace::new();
    for &x in values {
        trace.event("inc", [("x", Value::Int(x))]);
    }
    trace
}

#[test]
fn a_trace_the_spec_allows_is_accepted() {
    if !tlc_or_skip() {
        return;
    }
    assert_eq!(
        check().check(&incs(&[1, 2, 3])).expect("verdict"),
        Verdict::Accepted
    );
    assert_eq!(
        check().check(&Trace::new()).expect("verdict"),
        Verdict::Accepted
    );
}

#[test]
fn a_rejection_names_the_first_event_that_does_not_match() {
    if !tlc_or_skip() {
        return;
    }
    let verdict = check().check(&incs(&[1, 2, 4, 5])).expect("verdict");
    let Verdict::Rejected(rejection) = verdict else {
        panic!("a jump from 2 to 4 is not a step of the spec: {verdict:?}");
    };
    assert_eq!(rejection.index, 2);
    assert!(rejection.state.contains("/\\ x = 2"), "{}", rejection.state);
}

#[test]
fn a_rejection_at_the_first_event_has_no_state() {
    if !tlc_or_skip() {
        return;
    }
    let mut trace = incs(&[1]);
    trace.event("skip", Vec::<(String, Value)>::new());
    let verdict = check().check(&incs(&[7])).expect("verdict");
    assert_eq!(
        verdict,
        Verdict::Rejected(moonpool_tla::Rejection {
            index: 0,
            state: String::new()
        })
    );
    let Verdict::Rejected(rejection) = check().check(&trace).expect("verdict") else {
        panic!("an unknown event is rejected");
    };
    assert_eq!(rejection.index, 1);
}

#[test]
fn a_broken_invariant_names_it_and_the_matched_events() {
    if !tlc_or_skip() {
        return;
    }
    let verdict = check().check(&incs(&[1, 2, 3, 4, 5])).expect("verdict");
    let Verdict::InvariantViolated {
        invariant,
        matched,
        state,
    } = verdict
    else {
        panic!("x = 4 breaks BelowFour: {verdict:?}");
    };
    assert_eq!(invariant, "BelowFour");
    assert_eq!(matched, 4);
    assert!(state.contains("/\\ x = 4"), "{state}");
}

#[test]
fn a_spec_tlc_cannot_read_is_an_error() {
    if !tlc_or_skip() {
        return;
    }
    let error = TraceCheck::new(spec_dir(), "Missing")
        .check(&incs(&[1]))
        .expect_err("there is no Missing.cfg");
    assert!(matches!(error, CheckError::Io(_)), "{error}");
    let error = check()
        .cursor("nope")
        .check(&incs(&[1, 2]))
        .expect_err("the wrapper names a variable the spec lacks");
    assert!(matches!(error, CheckError::Tlc { .. }), "{error}");
}

#[test]
fn a_missing_tlc_is_reported() {
    let error = check()
        .tlc("/nonexistent/tlc")
        .check(&incs(&[1]))
        .expect_err("no such launcher");
    assert!(matches!(error, CheckError::TlcNotFound { .. }), "{error}");
    assert!(!check().tlc("/nonexistent/tlc").tlc_available());
}
