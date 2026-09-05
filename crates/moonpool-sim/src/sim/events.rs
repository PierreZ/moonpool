//! Deterministic event scheduling for the simulation engine.

use std::{
    cmp::Ordering,
    collections::{BTreeSet, BinaryHeap},
    error::Error,
    fmt,
    time::Duration,
};

use serde::Serialize;

use crate::network::sim::NetworkEvent;
pub use crate::storage::StorageOperation;
use crate::storage::sim::StorageEvent;

/// Events that can be scheduled in the simulation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Timer event for waking sleeping tasks.
    Timer {
        /// The unique identifier for the task to wake.
        task_id: u64,
    },
    /// A targeted network event.
    Network(NetworkEvent),
    /// A targeted storage event.
    Storage(StorageEvent),
    /// Shutdown event to wake all tasks for graceful termination.
    Shutdown,
    /// Process restart event: a rebooted process is ready to boot again.
    ProcessRestart {
        /// The IP address of the process to restart.
        ip: std::net::IpAddr,
    },
    /// Graceful shutdown initiated for a process.
    ProcessGracefulShutdown {
        /// The IP address of the process being gracefully shut down.
        ip: std::net::IpAddr,
        /// Grace period in milliseconds before force-kill.
        grace_period_ms: u64,
        /// Recovery delay in milliseconds after force-kill before restart.
        recovery_delay_ms: u64,
    },
    /// Force-kill a process: the task is aborted before any other side effect
    /// of the kill can wake it again.
    ProcessForceKill {
        /// The IP address of the process to force-kill.
        ip: std::net::IpAddr,
        /// Recovery delay in milliseconds before restart. `None` holds the
        /// process down indefinitely: no restart is scheduled until an
        /// explicit [`Event::ProcessRestart`] arrives (scripted fault
        /// injection via `FaultContext::restart`).
        recovery_delay_ms: Option<u64>,
        /// Why the process is dying, and what that costs its storage.
        cause: ProcessKillKind,
    },
}

/// Why a process task is force-killed, and what the kill does to its storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessKillKind {
    /// A graceful reboot's grace period expired: persistent storage survives.
    GracePeriodExpired,
    /// A [`RebootKind::Crash`](crate::RebootKind::Crash) reboot: unsynced
    /// storage state is lost.
    Crash,
    /// A [`RebootKind::CrashAndWipe`](crate::RebootKind::CrashAndWipe) reboot:
    /// every persistent file owned by the process is deleted as well.
    CrashAndWipe,
}

impl Event {
    /// Returns whether the event only maintains simulation infrastructure.
    #[must_use]
    pub fn is_infrastructure_event(&self) -> bool {
        matches!(self, Event::Network(event) if event.is_infrastructure())
            || matches!(self, Event::ProcessRestart { .. })
    }
}

/// Stable identifier assigned to a scheduled item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScheduleId(u64);

impl ScheduleId {
    /// Returns the deterministic sequence number backing this identifier.
    #[must_use]
    pub fn sequence(self) -> u64 {
        self.0
    }
}

/// Error returned when an item cannot be scheduled deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleError {
    /// Adding a relative delay overflowed [`Duration`].
    TimeOverflow,
    /// The global deterministic sequence space is exhausted.
    SequenceOverflow,
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimeOverflow => formatter.write_str("scheduled time overflowed Duration"),
            Self::SequenceOverflow => formatter.write_str("scheduler sequence space exhausted"),
        }
    }
}

impl Error for ScheduleError {}

/// An item scheduled for execution at a deterministic time and sequence.
#[derive(Debug, Clone)]
pub struct Scheduled<E> {
    time: Duration,
    value: E,
    id: ScheduleId,
}

impl<E> Scheduled<E> {
    /// Returns the scheduled execution time.
    #[must_use]
    pub fn time(&self) -> Duration {
        self.time
    }

    /// Returns the stable schedule identifier.
    #[must_use]
    pub fn id(&self) -> ScheduleId {
        self.id
    }

    /// Returns the deterministic sequence number.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.id.sequence()
    }

    /// Returns a reference to the scheduled value.
    #[must_use]
    pub fn value(&self) -> &E {
        &self.value
    }

    /// Consumes the entry and returns its value.
    #[must_use]
    pub fn into_value(self) -> E {
        self.value
    }
}

impl<E> PartialEq for Scheduled<E> {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time && self.id == other.id
    }
}

impl<E> Eq for Scheduled<E> {}

impl<E> PartialOrd for Scheduled<E> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<E> Ord for Scheduled<E> {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .time
            .cmp(&self.time)
            .then_with(|| other.id.cmp(&self.id))
    }
}

/// A deterministic delayed-item scheduler.
///
/// The scheduler exclusively owns logical time, global sequence allocation,
/// queue ordering, and cancellation. Cancelled entries are removed eagerly
/// without advancing logical time or leaving tombstones in the queue.
#[derive(Debug)]
pub struct Scheduler<E> {
    now: Duration,
    next_sequence: u64,
    heap: BinaryHeap<Scheduled<E>>,
    live: BTreeSet<ScheduleId>,
}

impl<E> Scheduler<E> {
    /// Creates an empty scheduler at time zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            now: Duration::ZERO,
            next_sequence: 0,
            heap: BinaryHeap::new(),
            live: BTreeSet::new(),
        }
    }

    /// Returns the current logical time.
    #[must_use]
    pub fn now(&self) -> Duration {
        self.now
    }

    /// Schedules a value after `delay` from the current logical time.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError::TimeOverflow`] if `delay` would exceed the
    /// representable duration, or [`ScheduleError::SequenceOverflow`] if every
    /// schedule identifier has been consumed.
    pub fn schedule_after(
        &mut self,
        delay: Duration,
        value: E,
    ) -> Result<ScheduleId, ScheduleError> {
        let time = self
            .now
            .checked_add(delay)
            .ok_or(ScheduleError::TimeOverflow)?;
        self.schedule_at(time, value)
    }

    /// Schedules a value at an absolute logical time.
    ///
    /// Times before the current logical time are clamped to the current time.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError::SequenceOverflow`] if every schedule identifier
    /// has been consumed.
    pub fn schedule_at(&mut self, time: Duration, value: E) -> Result<ScheduleId, ScheduleError> {
        let time = time.max(self.now);

        let next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(ScheduleError::SequenceOverflow)?;
        let id = ScheduleId(self.next_sequence);
        self.next_sequence = next_sequence;
        self.live.insert(id);
        self.heap.push(Scheduled { time, value, id });
        Ok(id)
    }

    /// Cancels a live scheduled item.
    ///
    /// Returns `true` only when the identifier still referred to a queued item.
    pub fn cancel(&mut self, id: ScheduleId) -> bool {
        if self.live.remove(&id) {
            self.heap.retain(|scheduled| scheduled.id != id);
            true
        } else {
            false
        }
    }

    /// Pops the next live item and advances logical time to its timestamp.
    ///
    /// The new time is also published to the simulation stream's logical
    /// clock, the time half of a determinism-canary fingerprint.
    pub fn pop(&mut self) -> Option<Scheduled<E>> {
        let scheduled = self.heap.pop()?;
        self.live.remove(&scheduled.id);
        self.now = scheduled.time;
        crate::sim::rng::note_logical_time(self.now);
        Some(scheduled)
    }

    /// Returns the number of live queued items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live.len()
    }

    /// Returns whether no live items remain queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Iterates over live queued items in unspecified heap order.
    pub fn iter(&self) -> impl Iterator<Item = &Scheduled<E>> {
        self.heap.iter()
    }
}

impl<E> Default for Scheduler<E> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{ScheduleError, Scheduled, Scheduler};
    use std::time::Duration;

    #[test]
    fn same_time_items_are_fifo() {
        let mut scheduler = Scheduler::new();
        scheduler
            .schedule_at(Duration::from_secs(1), "first")
            .expect("first item should schedule");
        scheduler
            .schedule_at(Duration::from_secs(1), "second")
            .expect("second item should schedule");
        scheduler
            .schedule_at(Duration::from_secs(1), "third")
            .expect("third item should schedule");

        let values = (0..3)
            .map(|_| {
                scheduler
                    .pop()
                    .expect("scheduled item should exist")
                    .into_value()
            })
            .collect::<Vec<_>>();
        assert_eq!(values, ["first", "second", "third"]);
    }

    #[test]
    fn cancellation_does_not_advance_time() {
        let mut scheduler = Scheduler::new();
        let cancelled = scheduler
            .schedule_at(Duration::from_secs(5), "cancelled")
            .expect("item should schedule");
        scheduler
            .schedule_at(Duration::from_secs(10), "live")
            .expect("item should schedule");

        assert!(scheduler.cancel(cancelled));
        assert!(!scheduler.cancel(cancelled));
        assert_eq!(scheduler.len(), 1);
        assert_eq!(scheduler.pop().map(Scheduled::into_value), Some("live"));
        assert_eq!(scheduler.now(), Duration::from_secs(10));
    }

    #[test]
    fn repeated_cancellation_does_not_retain_heap_entries() {
        let mut scheduler = Scheduler::new();
        for value in 0..10_000 {
            let id = scheduler
                .schedule_after(Duration::from_secs(1), value)
                .expect("item should schedule");
            assert!(scheduler.cancel(id));
        }

        assert!(scheduler.is_empty());
        assert!(scheduler.heap.is_empty());
        assert!(scheduler.pop().is_none());
        assert_eq!(scheduler.now(), Duration::ZERO);
    }

    #[test]
    fn scheduling_in_the_past_runs_now_without_regressing_time() {
        let mut scheduler = Scheduler::new();
        scheduler
            .schedule_at(Duration::from_secs(2), ())
            .expect("item should schedule");
        scheduler.pop().expect("scheduled item should exist");

        scheduler
            .schedule_at(Duration::from_secs(1), ())
            .expect("overdue item should schedule at now");
        let overdue = scheduler.pop().expect("overdue item should exist");
        assert_eq!(overdue.time(), Duration::from_secs(2));
        assert_eq!(scheduler.now(), Duration::from_secs(2));
    }

    #[test]
    fn time_and_sequence_overflow_are_rejected() {
        let mut scheduler = Scheduler::new();
        scheduler.now = Duration::MAX;
        assert_eq!(
            scheduler.schedule_after(Duration::from_nanos(1), ()),
            Err(ScheduleError::TimeOverflow)
        );

        scheduler.next_sequence = u64::MAX;
        assert_eq!(
            scheduler.schedule_at(Duration::MAX, ()),
            Err(ScheduleError::SequenceOverflow)
        );
    }
}
