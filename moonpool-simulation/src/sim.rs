use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    time::Duration,
};

use crate::{
    error::{SimulationError, SimulationResult},
    events::{Event, EventQueue, ScheduledEvent},
};

#[derive(Debug)]
struct SimInner {
    current_time: Duration,
    event_queue: EventQueue,
    next_sequence: u64,
}

impl SimInner {
    fn new() -> Self {
        Self {
            current_time: Duration::ZERO, // Starts at Duration::ZERO (0 unix time)
            event_queue: EventQueue::new(),
            next_sequence: 0,
        }
    }
}

/// The central simulation coordinator that manages time and event processing.
///
/// `SimWorld` owns all mutable simulation state and provides the main interface
/// for scheduling events and advancing simulation time. It uses a centralized
/// ownership model with handle-based access to avoid borrow checker conflicts.
#[derive(Debug)]
pub struct SimWorld {
    inner: Rc<RefCell<SimInner>>,
}

impl SimWorld {
    /// Creates a new simulation world.
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(SimInner::new())),
        }
    }

    /// Processes the next scheduled event and advances time.
    ///
    /// Returns `true` if more events are available for processing,
    /// `false` if this was the last event or if no events are available.
    pub fn step(&mut self) -> bool {
        let mut inner = self.inner.borrow_mut();

        if let Some(scheduled_event) = inner.event_queue.pop_earliest() {
            // Advance logical time to event timestamp
            inner.current_time = scheduled_event.time();

            // Process the event
            self.process_event(scheduled_event.into_event());

            // Return true if more events are available
            !inner.event_queue.is_empty()
        } else {
            // No more events to process
            false
        }
    }

    /// Processes all scheduled events until the queue is empty.
    pub fn run_until_empty(&mut self) {
        while self.step() {
            // Continue processing events
        }
    }

    /// Returns the current simulation time.
    pub fn current_time(&self) -> Duration {
        self.inner.borrow().current_time
    }

    /// Schedules an event to execute after the specified delay from the current time.
    pub fn schedule_event(&self, event: Event, delay: Duration) {
        let mut inner = self.inner.borrow_mut();
        let scheduled_time = inner.current_time + delay;
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        let scheduled_event = ScheduledEvent::new(scheduled_time, event, sequence);
        inner.event_queue.schedule(scheduled_event);
    }

    /// Schedules an event to execute at the specified absolute time.
    pub fn schedule_event_at(&self, event: Event, time: Duration) {
        let mut inner = self.inner.borrow_mut();
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        let scheduled_event = ScheduledEvent::new(time, event, sequence);
        inner.event_queue.schedule(scheduled_event);
    }

    /// Creates a weak reference to this simulation world.
    ///
    /// Weak references can be used to access the simulation without preventing
    /// it from being dropped, enabling handle-based access patterns.
    pub fn downgrade(&self) -> WeakSimWorld {
        WeakSimWorld {
            inner: Rc::downgrade(&self.inner),
        }
    }

    /// Returns `true` if there are events waiting to be processed.
    pub fn has_pending_events(&self) -> bool {
        !self.inner.borrow().event_queue.is_empty()
    }

    /// Returns the number of events waiting to be processed.
    pub fn pending_event_count(&self) -> usize {
        self.inner.borrow().event_queue.len()
    }

    fn process_event(&self, event: Event) {
        match event {
            Event::Wake { task_id: _ } => {
                // For Phase 1, we just acknowledge the wake event
                // In later phases, this will wake actual tasks
            }
        }
    }
}

impl Default for SimWorld {
    fn default() -> Self {
        Self::new()
    }
}

/// A weak reference to a simulation world.
///
/// This provides handle-based access to the simulation without holding
/// a strong reference that would prevent cleanup. All operations
/// return `SimulationResult` and will fail if the simulation has been dropped.
#[derive(Debug)]
pub struct WeakSimWorld {
    inner: Weak<RefCell<SimInner>>,
}

impl WeakSimWorld {
    /// Attempts to upgrade this weak reference to a strong reference.
    ///
    /// Returns `Err(SimulationError::SimulationShutdown)` if the simulation
    /// has been dropped.
    pub fn upgrade(&self) -> SimulationResult<SimWorld> {
        self.inner
            .upgrade()
            .map(|inner| SimWorld { inner })
            .ok_or(SimulationError::SimulationShutdown)
    }

    /// Returns the current simulation time.
    ///
    /// Returns `Err(SimulationError::SimulationShutdown)` if the simulation
    /// has been dropped.
    pub fn current_time(&self) -> SimulationResult<Duration> {
        let sim = self.upgrade()?;
        Ok(sim.current_time())
    }

    /// Schedules an event to execute after the specified delay from the current time.
    ///
    /// Returns `Err(SimulationError::SimulationShutdown)` if the simulation
    /// has been dropped.
    pub fn schedule_event(&self, event: Event, delay: Duration) -> SimulationResult<()> {
        let sim = self.upgrade()?;
        sim.schedule_event(event, delay);
        Ok(())
    }

    /// Schedules an event to execute at the specified absolute time.
    ///
    /// Returns `Err(SimulationError::SimulationShutdown)` if the simulation
    /// has been dropped.
    pub fn schedule_event_at(&self, event: Event, time: Duration) -> SimulationResult<()> {
        let sim = self.upgrade()?;
        sim.schedule_event_at(event, time);
        Ok(())
    }
}

impl Clone for WeakSimWorld {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_world_basic_lifecycle() {
        let mut sim = SimWorld::new();

        // Initial state
        assert_eq!(sim.current_time(), Duration::ZERO);
        assert!(!sim.has_pending_events());
        assert_eq!(sim.pending_event_count(), 0);

        // Schedule an event
        sim.schedule_event(Event::Wake { task_id: 1 }, Duration::from_millis(100));

        assert!(sim.has_pending_events());
        assert_eq!(sim.pending_event_count(), 1);
        assert_eq!(sim.current_time(), Duration::ZERO); // Time hasn't advanced yet

        // Process the event
        let has_more = sim.step();
        assert!(!has_more); // No more events after processing
        assert_eq!(sim.current_time(), Duration::from_millis(100)); // Time advanced
        assert!(!sim.has_pending_events());
        assert_eq!(sim.pending_event_count(), 0);
    }

    #[test]
    fn sim_world_multiple_events() {
        let mut sim = SimWorld::new();

        // Schedule multiple events
        sim.schedule_event(Event::Wake { task_id: 3 }, Duration::from_millis(300));
        sim.schedule_event(Event::Wake { task_id: 1 }, Duration::from_millis(100));
        sim.schedule_event(Event::Wake { task_id: 2 }, Duration::from_millis(200));

        assert_eq!(sim.pending_event_count(), 3);

        // Process events - should happen in time order
        assert!(sim.step()); // Event 1
        assert_eq!(sim.current_time(), Duration::from_millis(100));
        assert_eq!(sim.pending_event_count(), 2);

        assert!(sim.step()); // Event 2
        assert_eq!(sim.current_time(), Duration::from_millis(200));
        assert_eq!(sim.pending_event_count(), 1);

        assert!(!sim.step()); // Event 3 - last event
        assert_eq!(sim.current_time(), Duration::from_millis(300));
        assert_eq!(sim.pending_event_count(), 0);
    }

    #[test]
    fn sim_world_run_until_empty() {
        let mut sim = SimWorld::new();

        // Schedule multiple events
        sim.schedule_event(Event::Wake { task_id: 1 }, Duration::from_millis(100));
        sim.schedule_event(Event::Wake { task_id: 2 }, Duration::from_millis(200));
        sim.schedule_event(Event::Wake { task_id: 3 }, Duration::from_millis(300));

        // Run until all events are processed
        sim.run_until_empty();

        assert_eq!(sim.current_time(), Duration::from_millis(300));
        assert!(!sim.has_pending_events());
    }

    #[test]
    fn sim_world_schedule_at_specific_time() {
        let mut sim = SimWorld::new();

        // Schedule event at specific time (not relative to current time)
        sim.schedule_event_at(Event::Wake { task_id: 1 }, Duration::from_millis(500));

        // Current time is still zero
        assert_eq!(sim.current_time(), Duration::ZERO);

        // Process the event
        sim.step();

        // Time should jump to the scheduled time
        assert_eq!(sim.current_time(), Duration::from_millis(500));
    }

    #[test]
    fn weak_sim_world_lifecycle() {
        let sim = SimWorld::new();
        let weak = sim.downgrade();

        // Can upgrade and use weak reference
        assert_eq!(weak.current_time().unwrap(), Duration::ZERO);

        // Schedule event through weak reference
        weak.schedule_event(Event::Wake { task_id: 1 }, Duration::from_millis(100))
            .unwrap();

        // Verify event was scheduled
        assert!(sim.has_pending_events());

        // Drop the original simulation
        drop(sim);

        // Weak reference should now fail
        assert_eq!(
            weak.current_time(),
            Err(SimulationError::SimulationShutdown)
        );
        assert_eq!(
            weak.schedule_event(Event::Wake { task_id: 2 }, Duration::from_millis(200)),
            Err(SimulationError::SimulationShutdown)
        );
    }

    #[test]
    fn deterministic_event_ordering() {
        let mut sim = SimWorld::new();

        // Schedule events at the same time - should be processed in sequence order
        sim.schedule_event(Event::Wake { task_id: 2 }, Duration::from_millis(100));
        sim.schedule_event(Event::Wake { task_id: 1 }, Duration::from_millis(100));
        sim.schedule_event(Event::Wake { task_id: 3 }, Duration::from_millis(100));

        // All events are at the same time, but should be processed in the order they were scheduled
        // due to sequence numbers
        assert!(sim.step());
        assert_eq!(sim.current_time(), Duration::from_millis(100));
        assert!(sim.step()); // Should process in sequence order
        assert_eq!(sim.current_time(), Duration::from_millis(100)); // Time doesn't change
        assert!(!sim.step()); // Last event
        assert_eq!(sim.current_time(), Duration::from_millis(100));
    }
}
