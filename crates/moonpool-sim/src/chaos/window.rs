//! When the current run's chaos phase ended.
//!
//! The runner records the simulated time it ended chaos at
//! ([`SimWorld::enter_recovery_mode`](crate::SimWorld::enter_recovery_mode)
//! plus the injectors' shutdown), and clears it when an iteration starts.
//! Code under test and its harness read it with [`chaos_ended_at`]: a
//! process, a workload or an injector that times a scenario from the cutoff
//! (a fault struck at most some window into the quiet tail) needs the cutoff
//! itself when the chaos phase has no fixed length
//! ([`SimulationBuilder::chaos_until_run_ends`](crate::SimulationBuilder::chaos_until_run_ends)).
//!
//! The value lives in a thread-local, like buggify's state: the simulation
//! runs on one thread, and an explored child inherits it at the fork.

use std::cell::Cell;
use std::time::Duration;

thread_local! {
    static CHAOS_ENDED_AT: Cell<Option<Duration>> = const { Cell::new(None) };
}

/// The simulated time the current run's chaos phase ended, or `None` while
/// it runs (and in a run without a chaos phase, or outside any run).
#[must_use]
pub fn chaos_ended_at() -> Option<Duration> {
    CHAOS_ENDED_AT.with(Cell::get)
}

/// Whether the current run is still inside its chaos phase: the setup and
/// run phases until the cutoff.
#[must_use]
pub fn in_chaos() -> bool {
    chaos_ended_at().is_none()
}

/// Record the cutoff (`Some`) or clear it for a new iteration (`None`).
pub(crate) fn set_chaos_ended_at(at: Option<Duration>) {
    CHAOS_ENDED_AT.with(|cell| cell.set(at));
}
