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

#[test]
fn a_negative_integer_is_readable_by_tlc() {
    if !tlc_or_skip() {
        return;
    }
    let mut trace = Trace::new();
    trace.event("inc", [("x", Value::Int(1)), ("delta", Value::Int(-1))]);
    assert_eq!(check().check(&trace).expect("verdict"), Verdict::Accepted);
}

#[test]
fn parallel_checks_do_not_race() {
    if !tlc_or_skip() {
        return;
    }
    let threads: Vec<_> = (0..6)
        .map(|_| std::thread::spawn(|| check().check(&incs(&[1, 2]))))
        .collect();
    for thread in threads {
        let verdict = thread.join().expect("the check thread does not panic");
        assert_eq!(verdict.expect("verdict"), Verdict::Accepted);
    }
}

/// A copy of the counter spec folder with `cfg` as its configuration and
/// `extra` as one more file, removed on drop.
struct SpecCopy(PathBuf);

impl SpecCopy {
    fn new(name: &str, cfg: &str, extra: Option<&str>) -> Self {
        let dir =
            std::env::temp_dir().join(format!("moonpool-tla-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temporary folder");
        for file in ["Counter.tla", "CounterTrace.tla"] {
            std::fs::copy(spec_dir().join(file), dir.join(file)).expect("copy the spec");
        }
        std::fs::write(dir.join("CounterTrace.cfg"), cfg).expect("write the configuration");
        if let Some(extra) = extra {
            std::fs::write(dir.join(extra), "").expect("write the extra file");
        }
        Self(dir)
    }
}

impl Drop for SpecCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn the_end_invariant_must_be_the_last_invariant() {
    let early = SpecCopy::new(
        "early",
        "SPECIFICATION TraceSpec\nINVARIANT TraceUnfinished\nINVARIANT BelowFour\n",
        None,
    );
    let error = TraceCheck::new(&early.0, "CounterTrace")
        .check(&incs(&[1, 2, 3, 4]))
        .expect_err("BelowFour would never be reported");
    assert!(matches!(error, CheckError::Contract(_)), "{error}");

    let missing = SpecCopy::new(
        "missing",
        "SPECIFICATION TraceSpec\nINVARIANT BelowFour\n",
        None,
    );
    let error = TraceCheck::new(&missing.0, "CounterTrace")
        .check(&incs(&[1]))
        .expect_err("no trace could be accepted");
    assert!(matches!(error, CheckError::Contract(_)), "{error}");
}

#[test]
fn a_file_named_like_the_trace_module_is_refused() {
    let copy = SpecCopy::new(
        "reserved",
        "SPECIFICATION TraceSpec\nINVARIANT TraceUnfinished\n",
        Some("Trace.tla"),
    );
    let error = TraceCheck::new(&copy.0, "CounterTrace")
        .check(&incs(&[1]))
        .expect_err("the check writes Trace.tla");
    assert!(matches!(error, CheckError::Contract(_)), "{error}");
}
