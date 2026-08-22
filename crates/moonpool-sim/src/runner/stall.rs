//! Deterministic run-phase stall detection.

use std::time::Duration;

/// Default virtual-time budget for a single run phase.
///
/// The budget is intentionally generous so normal simulations never reach it.
/// It bounds workloads kept alive by a self-perpetuating timer, which the
/// empty-event-queue detector cannot identify.
pub(crate) const DEFAULT_RUN_TIME_BUDGET: Duration = Duration::from_hours(1);

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
        budget.merge(no_progress)
    }

    /// Reset the short no-progress window after requesting shutdown.
    pub(crate) fn reset_no_progress(&mut self) {
        self.no_progress_count = 0;
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
        guard.reset_no_progress();
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
