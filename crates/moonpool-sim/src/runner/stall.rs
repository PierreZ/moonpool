//! Deterministic run-phase stall detection.

use std::time::Duration;

/// Default virtual-time budget for a single run phase.
///
/// The budget is intentionally generous so normal simulations never reach it.
/// It bounds workloads kept alive by a self-perpetuating timer, which the
/// empty-event-queue detector cannot identify.
pub(crate) const DEFAULT_RUN_TIME_BUDGET: Duration = Duration::from_hours(1);

/// Maximum consecutive events at one logical time before the runner asks
/// actors to shut down. This is intentionally much larger than legitimate
/// same-time fan-out, while still turning a re-armed zero-duration sleep into
/// a deterministic failure instead of an infinite loop.
const SAME_TIME_EVENT_THRESHOLD: usize = 100_000;

/// Outcome of checking the run-phase stall guards.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StallOutcome {
    /// The run is making progress, or remains inside its shutdown grace window.
    Ok,
    /// A stall was first detected and graceful shutdown should begin.
    Breached,
    /// The run remains stalled after graceful shutdown.
    Deadlock,
}

impl StallOutcome {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Deadlock, _) | (_, Self::Deadlock) => Self::Deadlock,
            (Self::Breached, _) | (_, Self::Breached) => Self::Breached,
            _ => Self::Ok,
        }
    }
}

/// Tracks both forms of deterministic run-phase stalls.
///
/// Empty event queues are detected by counting consecutive iterations without
/// progress. Self-perpetuating timers are detected by the virtual-time budget.
/// Both guards use the same two-stage response: request graceful shutdown on
/// the first breach, then report a deadlock if the stall persists.
pub(crate) struct RunStallGuard {
    no_progress_count: usize,
    no_progress_threshold: usize,
    same_time_count: usize,
    same_time_threshold: usize,
    last_time: Duration,
    run_start: Duration,
    run_time_budget: Duration,
    budget_breach_time: Option<Duration>,
    seed: u64,
    iteration: usize,
}

impl RunStallGuard {
    /// Create the guards for one run phase.
    pub(crate) fn new(
        run_start: Duration,
        run_time_budget: Duration,
        seed: u64,
        iteration: usize,
    ) -> Self {
        Self {
            no_progress_count: 0,
            no_progress_threshold: 3,
            same_time_count: 0,
            same_time_threshold: SAME_TIME_EVENT_THRESHOLD,
            last_time: run_start,
            run_start,
            run_time_budget,
            budget_breach_time: None,
            seed,
            iteration,
        }
    }

    /// Evaluate both guards after one cooperative-loop iteration.
    pub(crate) fn evaluate(
        &mut self,
        sim: &crate::sim::SimWorld,
        shutdown_triggered: bool,
        current_active: usize,
        initial_active: usize,
        initial_event_count: usize,
    ) -> StallOutcome {
        let now = sim.current_time();
        let budget = self.check_budget(current_active, now);
        if budget == StallOutcome::Breached {
            self.budget_breach_time = Some(now);
        }

        let no_progress = self.check_no_progress(
            shutdown_triggered,
            current_active,
            initial_active,
            sim.pending_event_count(),
            initial_event_count,
        );
        let same_time = self.check_same_time(
            shutdown_triggered,
            current_active,
            initial_active,
            initial_event_count,
            now,
        );
        budget.merge(no_progress).merge(same_time)
    }

    /// Reset phase-local counters after requesting shutdown.
    pub(crate) fn reset_after_shutdown(&mut self) {
        self.no_progress_count = 0;
        self.same_time_count = 0;
    }

    fn check_same_time(
        &mut self,
        shutdown_triggered: bool,
        current_active: usize,
        initial_active: usize,
        initial_event_count: usize,
        now: Duration,
    ) -> StallOutcome {
        if current_active == 0 || initial_event_count == 0 {
            return StallOutcome::Ok;
        }

        // Virtual-time advancement or actor completion is genuine scheduler
        // progress. `shutdown_triggered` remains true across this reset, so a
        // later same-time loop takes the second-stage deadlock path directly
        // instead of repeatedly requesting shutdown.
        if current_active < initial_active || now > self.last_time {
            self.same_time_count = 0;
        }
        self.last_time = now;
        self.same_time_count += 1;

        if self.same_time_count <= self.same_time_threshold {
            return StallOutcome::Ok;
        }
        if shutdown_triggered {
            tracing::error!(
                "DEADLOCK detected on iteration {} with seed {}: {} tasks remained active after {} same-time events following shutdown",
                self.iteration,
                self.seed,
                current_active,
                self.same_time_count,
            );
            return StallOutcome::Deadlock;
        }
        tracing::warn!(
            "{} same-time events on iteration {} with seed {} left {} tasks active. Triggering shutdown to break a zero-duration timer loop.",
            self.same_time_count,
            self.iteration,
            self.seed,
            current_active,
        );
        StallOutcome::Breached
    }

    fn check_no_progress(
        &mut self,
        shutdown_triggered: bool,
        current_active: usize,
        initial_active: usize,
        event_count: usize,
        initial_event_count: usize,
    ) -> StallOutcome {
        if event_count == 0 && current_active == initial_active && initial_event_count == 0 {
            self.no_progress_count += 1;
        } else {
            self.no_progress_count = 0;
        }

        if self.no_progress_count <= self.no_progress_threshold {
            return StallOutcome::Ok;
        }
        if shutdown_triggered {
            tracing::error!(
                "DEADLOCK detected on iteration {} with seed {}: {} tasks remaining after {} no-progress iterations",
                self.iteration,
                self.seed,
                current_active,
                self.no_progress_count,
            );
            return StallOutcome::Deadlock;
        }
        tracing::warn!(
            "No progress detected on iteration {} with seed {}: {} tasks remaining. Triggering shutdown to unblock workloads.",
            self.iteration,
            self.seed,
            current_active,
        );
        StallOutcome::Breached
    }

    fn check_budget(&self, current_active: usize, now: Duration) -> StallOutcome {
        if current_active == 0 {
            return StallOutcome::Ok;
        }

        let run_elapsed = now.saturating_sub(self.run_start);
        match self.budget_breach_time {
            None if run_elapsed > self.run_time_budget => {
                tracing::warn!(
                    "Run-phase virtual-time budget exceeded on iteration {} with seed {}: simulated time advanced {:?} (budget {:?}) with {} workload(s) still running. Triggering shutdown to unblock workloads.",
                    self.iteration,
                    self.seed,
                    run_elapsed,
                    self.run_time_budget,
                    current_active,
                );
                StallOutcome::Breached
            }
            Some(breach) if now.saturating_sub(breach) > self.run_time_budget => {
                tracing::error!(
                    "DEADLOCK detected on iteration {} with seed {}: run-phase virtual time advanced {:?} (budget {:?}) and kept climbing for another {:?} after shutdown with {} workload(s) still running — self-perpetuating timer making no workload progress",
                    self.iteration,
                    self.seed,
                    run_elapsed,
                    self.run_time_budget,
                    now.saturating_sub(breach),
                    current_active,
                );
                StallOutcome::Deadlock
            }
            _ => StallOutcome::Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RunStallGuard, StallOutcome};
    use std::time::Duration;

    fn guard() -> RunStallGuard {
        RunStallGuard::new(Duration::ZERO, Duration::from_secs(10), 7, 2)
    }

    #[test]
    fn no_progress_uses_two_stage_escalation() {
        let mut guard = guard();

        for _ in 0..3 {
            assert_eq!(guard.check_no_progress(false, 1, 1, 0, 0), StallOutcome::Ok);
        }
        assert_eq!(
            guard.check_no_progress(false, 1, 1, 0, 0),
            StallOutcome::Breached
        );
        guard.reset_after_shutdown();
        for _ in 0..3 {
            assert_eq!(guard.check_no_progress(true, 1, 1, 0, 0), StallOutcome::Ok);
        }
        assert_eq!(
            guard.check_no_progress(true, 1, 1, 0, 0),
            StallOutcome::Deadlock
        );
    }

    #[test]
    fn progress_resets_the_no_progress_window() {
        let mut guard = guard();
        for _ in 0..3 {
            assert_eq!(guard.check_no_progress(false, 1, 1, 0, 0), StallOutcome::Ok);
        }

        assert_eq!(guard.check_no_progress(false, 0, 1, 0, 0), StallOutcome::Ok);
        assert_eq!(guard.check_no_progress(false, 1, 1, 0, 0), StallOutcome::Ok);
    }

    #[test]
    fn same_time_events_use_two_stage_escalation() {
        let mut guard = guard();
        guard.same_time_threshold = 2;

        assert_eq!(
            guard.check_same_time(false, 1, 1, 1, Duration::ZERO),
            StallOutcome::Ok
        );
        assert_eq!(
            guard.check_same_time(false, 1, 1, 1, Duration::ZERO),
            StallOutcome::Ok
        );
        assert_eq!(
            guard.check_same_time(false, 1, 1, 1, Duration::ZERO),
            StallOutcome::Breached
        );

        guard.reset_after_shutdown();
        assert_eq!(
            guard.check_same_time(true, 1, 1, 1, Duration::from_nanos(1)),
            StallOutcome::Ok
        );
        assert_eq!(
            guard.check_same_time(true, 1, 1, 1, Duration::from_nanos(2)),
            StallOutcome::Ok
        );
        assert_eq!(
            guard.check_same_time(true, 1, 1, 1, Duration::from_nanos(2)),
            StallOutcome::Ok
        );
        assert_eq!(
            guard.check_same_time(true, 1, 1, 1, Duration::from_nanos(2)),
            StallOutcome::Deadlock
        );
    }

    #[test]
    fn virtual_time_budget_allows_a_full_shutdown_grace_window() {
        let mut guard = guard();

        assert_eq!(
            guard.check_budget(1, Duration::from_secs(10)),
            StallOutcome::Ok
        );
        assert_eq!(
            guard.check_budget(1, Duration::from_secs(11)),
            StallOutcome::Breached
        );
        guard.budget_breach_time = Some(Duration::from_secs(11));
        assert_eq!(
            guard.check_budget(1, Duration::from_secs(21)),
            StallOutcome::Ok
        );
        assert_eq!(
            guard.check_budget(1, Duration::from_secs(22)),
            StallOutcome::Deadlock
        );
    }
}
