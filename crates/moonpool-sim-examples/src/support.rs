//! Plumbing shared by the examples and their `sim-*` binaries.
//!
//! Nothing here is part of what an example demonstrates: it is the error
//! wrapping, shutdown racing, attrition preset and exit-code handling that
//! every example would otherwise repeat verbatim.

use std::future::Future;
use std::process;

use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, SimContext, SimulationError,
    SimulationReport, SimulationResult,
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

/// Print the report and exit non-zero when any seed failed.
pub fn finish_or_exit_on_failing_seeds(report: &SimulationReport) {
    report.eprint();

    if !report.seeds_failing.is_empty() {
        eprintln!(
            "ERROR: {} seeds failed: {:?}",
            report.seeds_failing.len(),
            report.seeds_failing
        );
        process::exit(1);
    }
}

/// Print the report and exit non-zero when exploration ran no timeline.
pub fn finish_or_exit_if_unexplored(report: &SimulationReport) {
    report.eprint();

    if report
        .exploration
        .as_ref()
        .is_some_and(|e| e.total_timelines == 0)
    {
        eprintln!("ERROR: no timelines explored");
        process::exit(1);
    }
}
