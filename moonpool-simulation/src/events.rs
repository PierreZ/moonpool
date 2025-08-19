use std::{cmp::Ordering, collections::BinaryHeap, time::Duration};

/// Events that can be scheduled in the simulation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A wake event for a specific task.
    Wake {
        /// The unique identifier for the task to wake.
        task_id: u64,
    },

    // Phase 2b network events
    /// Listener bind operation completed
    BindComplete {
        /// Unique identifier for the listener
        listener_id: u64,
    },
    /// Connection establishment completed
    ConnectionReady {
        /// Unique identifier for the connection
        connection_id: u64,
    },
    /// Data delivery to connection's receive buffer
    DataDelivery {
        /// Unique identifier for the connection
        connection_id: u64,
        /// The data being delivered
        data: Vec<u8>,
    },
}

/// An event scheduled for execution at a specific simulation time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledEvent {
    time: Duration,
    event: Event,
    sequence: u64, // For deterministic ordering
}

impl ScheduledEvent {
    /// Creates a new scheduled event.
    pub fn new(time: Duration, event: Event, sequence: u64) -> Self {
        Self {
            time,
            event,
            sequence,
        }
    }

    /// Returns the scheduled execution time.
    pub fn time(&self) -> Duration {
        self.time
    }

    /// Returns a reference to the event.
    pub fn event(&self) -> &Event {
        &self.event
    }

    /// Consumes the scheduled event and returns the event.
    pub fn into_event(self) -> Event {
        self.event
    }
}

impl PartialOrd for ScheduledEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max heap, but we want earliest time first
        // So we reverse the time comparison
        match other.time.cmp(&self.time) {
            Ordering::Equal => {
                // For events at the same time, use sequence number for deterministic ordering
                // Earlier sequence numbers should be processed first (also reversed for max heap)
                other.sequence.cmp(&self.sequence)
            }
            other => other,
        }
    }
}

/// A priority queue for scheduling events in chronological order.
///
/// Events are processed in time order, with deterministic ordering for events
/// scheduled at the same time using sequence numbers.
#[derive(Debug)]
pub struct EventQueue {
    heap: BinaryHeap<ScheduledEvent>,
}

impl EventQueue {
    /// Creates a new empty event queue.
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
        }
    }

    /// Schedules an event for execution.
    pub fn schedule(&mut self, event: ScheduledEvent) {
        self.heap.push(event);
    }

    /// Removes and returns the earliest scheduled event.
    pub fn pop_earliest(&mut self) -> Option<ScheduledEvent> {
        self.heap.pop()
    }

    /// Returns a reference to the earliest scheduled event without removing it.
    pub fn peek_earliest(&self) -> Option<&ScheduledEvent> {
        self.heap.peek()
    }

    /// Returns `true` if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Returns the number of events in the queue.
    pub fn len(&self) -> usize {
        self.heap.len()
    }
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_queue_ordering() {
        let mut queue = EventQueue::new();

        // Schedule events in random order
        queue.schedule(ScheduledEvent::new(
            Duration::from_millis(300),
            Event::Wake { task_id: 3 },
            2,
        ));
        queue.schedule(ScheduledEvent::new(
            Duration::from_millis(100),
            Event::Wake { task_id: 1 },
            0,
        ));
        queue.schedule(ScheduledEvent::new(
            Duration::from_millis(200),
            Event::Wake { task_id: 2 },
            1,
        ));

        // Should pop in time order
        let event1 = queue.pop_earliest().unwrap();
        assert_eq!(event1.time(), Duration::from_millis(100));
        assert_eq!(event1.event(), &Event::Wake { task_id: 1 });

        let event2 = queue.pop_earliest().unwrap();
        assert_eq!(event2.time(), Duration::from_millis(200));
        assert_eq!(event2.event(), &Event::Wake { task_id: 2 });

        let event3 = queue.pop_earliest().unwrap();
        assert_eq!(event3.time(), Duration::from_millis(300));
        assert_eq!(event3.event(), &Event::Wake { task_id: 3 });

        assert!(queue.is_empty());
    }

    #[test]
    fn same_time_deterministic_ordering() {
        let mut queue = EventQueue::new();
        let same_time = Duration::from_millis(100);

        // Schedule multiple events at the same time with different sequence numbers
        queue.schedule(ScheduledEvent::new(
            same_time,
            Event::Wake { task_id: 3 },
            2, // Later sequence
        ));
        queue.schedule(ScheduledEvent::new(
            same_time,
            Event::Wake { task_id: 1 },
            0, // Earlier sequence
        ));
        queue.schedule(ScheduledEvent::new(
            same_time,
            Event::Wake { task_id: 2 },
            1, // Middle sequence
        ));

        // Should pop in sequence order when times are equal
        let event1 = queue.pop_earliest().unwrap();
        assert_eq!(event1.event(), &Event::Wake { task_id: 1 });
        assert_eq!(event1.sequence, 0);

        let event2 = queue.pop_earliest().unwrap();
        assert_eq!(event2.event(), &Event::Wake { task_id: 2 });
        assert_eq!(event2.sequence, 1);

        let event3 = queue.pop_earliest().unwrap();
        assert_eq!(event3.event(), &Event::Wake { task_id: 3 });
        assert_eq!(event3.sequence, 2);

        assert!(queue.is_empty());
    }
}
