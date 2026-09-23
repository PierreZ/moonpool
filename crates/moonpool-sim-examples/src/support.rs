//! Plumbing shared by the examples and their `sim-*` binaries.
//!
//! Nothing here is part of what an example demonstrates: it is the error
//! wrapping, shutdown racing, attrition preset and exit-code handling that
//! every example would otherwise repeat verbatim.

use std::future::Future;
use std::process;

use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, ExplorationReport, SimContext,
    SimulationError, SimulationReport, SimulationResult,
};

/// Wrap a message as [`SimulationError::InvalidState`].
pub fn invalid_state(message: impl Into<String>) -> SimulationError {
    SimulationError::InvalidState(message.into())
}

/// Resolve the IP of the process registered under `name`.
///
/// # Errors
///
/// Returns [`SimulationError::InvalidState`] when no such process exists.
pub fn peer_ip(ctx: &SimContext, name: &str) -> SimulationResult<String> {
    ctx.peer(name)
        .ok_or_else(|| invalid_state(format!("{name} process not found")))
}

/// Drive `future` unless the context's shutdown token fires first.
///
/// Returns `None` on shutdown. The race is `biased` toward `future`, so no
/// scheduling randomness is drawn.
pub async fn unless_shutdown<F: Future>(ctx: &SimContext, future: F) -> Option<F::Output> {
    moonpool_sim::select! {
        biased;
        output = future => Some(output),
        () = ctx.shutdown().cancelled() => None,
    }
}

/// Random-mode attrition that reboots at most `max_dead` victims at a time
/// within `scope`: 30% graceful, 50% crash, 20% wipe, default delays.
#[must_use]
pub fn reboot_attrition(max_dead: usize, scope: AttritionScope) -> Chaos {
    Chaos::Attrition {
        config: Attrition {
            max_dead,
            prob_graceful: 0.3,
            prob_crash: 0.5,
            prob_wipe: 0.2,
            recovery_delay_ms: None,
            grace_period_ms: None,
            scope,
            victims: AttritionVictims::Any,
        },
        mode: ChaosMode::Random,
    }
}

/// Print the report and exit non-zero when the run found a failure.
///
/// Gates on [`SimulationReport::is_failure`], not on `seeds_failing` alone: an
/// always-violation or an assertion-table overflow is recorded at run level
/// and may fail no individual seed.
pub fn finish_or_exit_on_failure(report: &SimulationReport) {
    report.eprint();

    if report.is_failure() {
        eprintln!(
            "ERROR: {} seeds failed {:?}, {} assertion violations, {} dropped assertion allocations",
            report.seeds_failing.len(),
            report.seeds_failing,
            report.assertion_violations.len(),
            report.dropped_assertion_allocations
        );
        process::exit(1);
    }
}

/// Print the report and exit non-zero unless exploration found the planted
/// bug.
///
/// For a planted-bug example (maze) whose seeds are *expected* to fail.
/// Running a timeline is not enough: an exploration that expands one level
/// and stalls, or never reaches the bug, must not exit 0.
pub fn finish_or_exit_unless_bug_found(report: &SimulationReport) {
    report.eprint();

    let exploration = explored(report);
    if exploration.bugs_found == 0 {
        eprintln!(
            "ERROR: exploration ran {} timelines but never reached the planted bug",
            exploration.total_timelines
        );
        process::exit(1);
    }
}

/// Print the report and exit non-zero unless exploration either found the
/// planted bug or drove the numeric assertion `msg` to a watermark of at least
/// `floor`.
///
/// For a planted-bug example (dungeon) whose bug is deep enough that a CI
/// budget does not reliably reach it: the depth the exploration did reach is
/// then the regression signal, so a stall at a shallow level fails the run.
pub fn finish_or_exit_below_watermark(report: &SimulationReport, msg: &str, floor: i64) {
    report.eprint();

    let exploration = explored(report);
    if exploration.bugs_found > 0 {
        return;
    }
    let watermark = report
        .assertion_details
        .iter()
        .find(|detail| detail.msg == msg)
        .map(|detail| detail.watermark);
    match watermark {
        Some(watermark) if watermark >= floor => {}
        Some(watermark) => {
            eprintln!(
                "ERROR: exploration ran {} timelines, found no bug, and drove \"{msg}\" only to {watermark} (floor {floor})",
                exploration.total_timelines
            );
            process::exit(1);
        }
        None => {
            eprintln!("ERROR: exploration never evaluated \"{msg}\"");
            process::exit(1);
        }
    }
}

/// The exploration section of `report`, exiting non-zero when exploration did
/// not run or ran no timeline.
fn explored(report: &SimulationReport) -> &ExplorationReport {
    let Some(exploration) = report.exploration.as_ref() else {
        eprintln!("ERROR: exploration did not run");
        process::exit(1);
    };
    if exploration.total_timelines == 0 {
        eprintln!("ERROR: no timelines explored");
        process::exit(1);
    }
    exploration
}
