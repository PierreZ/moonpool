use std::{cmp::Ordering, collections::BinaryHeap, time::Duration};

/// Events that can be scheduled in the simulation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Timer event for waking sleeping tasks
    Timer {
        /// The unique identifier for the task to wake.
        task_id: u64,
    },

    /// Network data operations
    Network {
        /// The connection involved
        connection_id: u64,
        /// The operation type
        operation: NetworkOperation,
    },

    /// Connection state changes
    Connection {
        /// The connection or listener ID
        id: u64,
        /// The state change type  
        state: ConnectionStateChange,
    },

    /// Shutdown event to wake all tasks for graceful termination
    Shutdown,
}

impl Event {
    /// Determines if this event is purely infrastructural (not workload-related).
    ///
    /// Infrastructure events maintain simulation state but don't represent actual
    /// application work. These events can be safely ignored when determining if
    /// a simulation should terminate after workloads complete.
    pub fn is_infrastructure_event(&self) -> bool {
        matches!(
            self,
            Event::Connection {
                state: ConnectionStateChange::PartitionRestore
                    | ConnectionStateChange::SendPartitionClear
                    | ConnectionStateChange::RecvPartitionClear,
                ..
            } // Could add other infrastructure events here if needed:
              // | Event::Connection { state: ConnectionStateChange::ClogClear, .. }
        )
    }
}

/// Network data operations
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkOperation {
    /// Deliver data to connection's receive buffer
    DataDelivery {
        /// The data bytes to deliver
        data: Vec<u8>,
    },
    /// Process next message from connection's send buffer
    ProcessSendBuffer,
}

/// Connection state changes
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStateChange {
    /// Listener bind operation completed
    BindComplete,
    /// Connection establishment completed
    ConnectionReady,
    /// Clear clog for a connection
    ClogClear,
    /// Restore network partition between IPs
    PartitionRestore,
    /// Clear send partition for an IP
    SendPartitionClear,
    /// Clear receive partition for an IP
    RecvPartitionClear,
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

    /// Checks if the queue contains only infrastructure events (no workload events).
    ///
    /// Infrastructure events are those that maintain simulation state but don't
    /// represent actual application work (like connection restoration).
    /// Returns true if empty or contains only infrastructure events.
    pub fn has_only_infrastructure_events(&self) -> bool {
        self.heap
            .iter()
            .all(|scheduled_event| scheduled_event.event().is_infrastructure_event())
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
    fn test_infrastructure_event_detection() {
        // Test Event::is_infrastructure_event() method
        let restore_event = Event::Connection {
            id: 1,
            state: ConnectionStateChange::PartitionRestore,
        };
        assert!(restore_event.is_infrastructure_event());

        let timer_event = Event::Timer { task_id: 1 };
        assert!(!timer_event.is_infrastructure_event());

        let network_event = Event::Network {
            connection_id: 1,
            operation: NetworkOperation::DataDelivery {
                data: vec![1, 2, 3],
            },
        };
        assert!(!network_event.is_infrastructure_event());

        let shutdown_event = Event::Shutdown;
        assert!(!shutdown_event.is_infrastructure_event());

        // Test EventQueue::has_only_infrastructure_events() method
        let mut queue = EventQueue::new();

        // Empty queue should be considered "only infrastructure"
        assert!(queue.has_only_infrastructure_events());

        // Queue with only ConnectionRestore events
        queue.schedule(ScheduledEvent::new(
            Duration::from_secs(1),
            restore_event,
            1,
        ));
        assert!(queue.has_only_infrastructure_events());

        // Queue with workload events should return false
        queue.schedule(ScheduledEvent::new(Duration::from_secs(2), timer_event, 2));
        assert!(!queue.has_only_infrastructure_events());

        // Queue with network events should return false
        let mut queue2 = EventQueue::new();
        queue2.schedule(ScheduledEvent::new(
            Duration::from_secs(1),
            network_event,
            1,
        ));
        assert!(!queue2.has_only_infrastructure_events());
    }

    #[test]
    fn event_queue_ordering() {
        let mut queue = EventQueue::new();

        // Schedule events in random order
        queue.schedule(ScheduledEvent::new(
            Duration::from_millis(300),
            Event::Timer { task_id: 3 },
            2,
        ));
        queue.schedule(ScheduledEvent::new(
            Duration::from_millis(100),
            Event::Timer { task_id: 1 },
            0,
        ));
        queue.schedule(ScheduledEvent::new(
            Duration::from_millis(200),
            Event::Timer { task_id: 2 },
            1,
        ));

        // Should pop in time order
        let event1 = queue.pop_earliest().unwrap();
        assert_eq!(event1.time(), Duration::from_millis(100));
        assert_eq!(event1.event(), &Event::Timer { task_id: 1 });

        let event2 = queue.pop_earliest().unwrap();
        assert_eq!(event2.time(), Duration::from_millis(200));
        assert_eq!(event2.event(), &Event::Timer { task_id: 2 });

        let event3 = queue.pop_earliest().unwrap();
        assert_eq!(event3.time(), Duration::from_millis(300));
        assert_eq!(event3.event(), &Event::Timer { task_id: 3 });

        assert!(queue.is_empty());
    }

    #[test]
    fn same_time_deterministic_ordering() {
        let mut queue = EventQueue::new();
        let same_time = Duration::from_millis(100);

        // Schedule multiple events at the same time with different sequence numbers
        queue.schedule(ScheduledEvent::new(
            same_time,
            Event::Timer { task_id: 3 },
            2, // Later sequence
        ));
        queue.schedule(ScheduledEvent::new(
            same_time,
            Event::Timer { task_id: 1 },
            0, // Earlier sequence
        ));
        queue.schedule(ScheduledEvent::new(
            same_time,
            Event::Timer { task_id: 2 },
            1, // Middle sequence
        ));

        // Should pop in sequence order when times are equal
        let event1 = queue.pop_earliest().unwrap();
        assert_eq!(event1.event(), &Event::Timer { task_id: 1 });
        assert_eq!(event1.sequence, 0);

        let event2 = queue.pop_earliest().unwrap();
        assert_eq!(event2.event(), &Event::Timer { task_id: 2 });
        assert_eq!(event2.sequence, 1);

        let event3 = queue.pop_earliest().unwrap();
        assert_eq!(event3.event(), &Event::Timer { task_id: 3 });
        assert_eq!(event3.sequence, 2);

        assert!(queue.is_empty());
    }
}
