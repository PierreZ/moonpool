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
/// `extra` as more files (name, content), removed on drop.
struct SpecCopy(PathBuf);

impl SpecCopy {
    fn new(name: &str, cfg: &str, extra: &[(&str, &str)]) -> Self {
        let dir =
            std::env::temp_dir().join(format!("moonpool-tla-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temporary folder");
        for file in ["Counter.tla", "CounterTrace.tla"] {
            std::fs::copy(spec_dir().join(file), dir.join(file)).expect("copy the spec");
        }
        std::fs::write(dir.join("CounterTrace.cfg"), cfg).expect("write the configuration");
        for (file, content) in extra {
            std::fs::write(dir.join(file), content).expect("write the extra file");
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
        &[],
    );
    let error = TraceCheck::new(&early.0, "CounterTrace")
        .check(&incs(&[1, 2, 3, 4]))
        .expect_err("BelowFour would never be reported");
    assert!(matches!(error, CheckError::Contract(_)), "{error}");

    let missing = SpecCopy::new(
        "missing",
        "SPECIFICATION TraceSpec\nINVARIANT BelowFour\n",
        &[],
    );
    let error = TraceCheck::new(&missing.0, "CounterTrace")
        .check(&incs(&[1]))
        .expect_err("no trace could be accepted");
    assert!(matches!(error, CheckError::Contract(_)), "{error}");
}

#[test]
fn a_file_named_like_the_wrapper_is_refused() {
    for file in ["MoonpoolTraceCheck.tla", "MoonpoolTraceCheck.cfg"] {
        let copy = SpecCopy::new(
            "reserved",
            "SPECIFICATION TraceSpec\nINVARIANT TraceUnfinished\n",
            &[(file, "")],
        );
        let error = TraceCheck::new(&copy.0, "CounterTrace")
            .check(&incs(&[1]))
            .expect_err("the check writes the wrapper");
        assert!(matches!(error, CheckError::Contract(_)), "{error}");
    }
}

#[test]
fn a_sample_trace_beside_the_spec_is_replaced() {
    if !tlc_or_skip() {
        return;
    }
    // A sample that would reject every trace: the check must not read it.
    let copy = SpecCopy::new(
        "sample",
        "SPECIFICATION TraceSpec\nINVARIANT TraceUnfinished\n",
        &[(
            "Trace.tla",
            "---- MODULE Trace ----\nTrace == <<[event |-> \"skip\"]>>\n====\n",
        )],
    );
    assert_eq!(
        TraceCheck::new(&copy.0, "CounterTrace")
            .check(&incs(&[1, 2]))
            .expect("verdict"),
        Verdict::Accepted
    );
}

#[test]
fn a_user_constraint_does_not_hide_the_rejected_event() {
    if !tlc_or_skip() {
        return;
    }
    // A configuration names a constraint: the trace spec gets one more
    // definition before its end line.
    let trace_spec = std::fs::read_to_string(spec_dir().join("CounterTrace.tla"))
        .expect("the trace spec")
        .replacen("\n====", "\nSmall == x < 2\n====", 1);
    let copy = SpecCopy::new(
        "constraint",
        "SPECIFICATION TraceSpec\nCONSTRAINT Small\nINVARIANT TraceUnfinished\n",
        &[("CounterTrace.tla", &trace_spec)],
    );
    let verdict = TraceCheck::new(&copy.0, "CounterTrace")
        .check(&incs(&[1, 2, 9]))
        .expect("verdict");
    let Verdict::Rejected(rejection) = verdict else {
        panic!("a jump from 2 to 9 is not a step of the spec: {verdict:?}");
    };
    assert_eq!(rejection.index, 2);
}

#[test]
fn a_configuration_with_block_comments_is_read() {
    if !tlc_or_skip() {
        return;
    }
    let copy = SpecCopy::new(
        "comments",
        "(* the counter (* nested *) INVARIANT Hidden *)\n\
         SPECIFICATION TraceSpec\n\
         INVARIANT BelowFour (* the model *)\n\
         INVARIANT TraceUnfinished\n",
        &[],
    );
    assert_eq!(
        TraceCheck::new(&copy.0, "CounterTrace")
            .check(&incs(&[1, 2]))
            .expect("verdict"),
        Verdict::Accepted
    );
}

#[test]
fn a_spec_may_define_its_own_int() {
    if !tlc_or_skip() {
        return;
    }
    // The trace spec extends the trace module. The trace module keeps
    // `Integers` local, so the spec's own `Int` is the only one, and it
    // forbids x = 2.
    let copy = SpecCopy::new(
        "int",
        "SPECIFICATION TraceSpec\nINVARIANT TraceUnfinished\n",
        &[(
            "Counter.tla",
            "---- MODULE Counter ----\n\
             EXTENDS Naturals\n\
             VARIABLE x\n\
             Int == 0..1\n\
             Init == x = 0\n\
             Inc == x < 5 /\\ x' = x + 1 /\\ x' \\in Int\n\
             BelowFour == x < 4\n\
             ====\n",
        )],
    );
    let mut trace = incs(&[1, 2]);
    trace.event("inc", [("x", Value::Int(3)), ("delta", Value::Int(-1))]);
    let verdict = TraceCheck::new(&copy.0, "CounterTrace")
        .check(&trace)
        .expect("verdict");
    let Verdict::Rejected(rejection) = verdict else {
        panic!("the spec's Int forbids x = 2: {verdict:?}");
    };
    assert_eq!(rejection.index, 1);
}

#[test]
fn a_temporary_folder_with_a_space_works() {
    if !tlc_or_skip() {
        return;
    }
    let root = std::env::temp_dir().join(format!("moonpool tla space {}", std::process::id()));
    std::fs::create_dir_all(&root).expect("temporary folder");
    let verdict = check().temp_dir(&root).check(&incs(&[1, 2]));
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(verdict.expect("verdict"), Verdict::Accepted);
}
