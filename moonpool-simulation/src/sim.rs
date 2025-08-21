use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::{Rc, Weak},
    task::Waker,
    time::Duration,
};

use crate::{
    error::{SimulationError, SimulationResult},
    events::{Event, EventQueue, ScheduledEvent},
    network::{
        NetworkConfiguration,
        sim::{ConnectionId, ListenerId, SimNetworkProvider},
    },
    rng::{reset_sim_rng, set_sim_seed},
    sleep::SleepFuture,
};
use std::collections::VecDeque;

/// Internal connection state for simulation
#[derive(Debug)]
struct ConnectionState {
    #[allow(dead_code)] // Will be used for routing and debugging in future phases
    id: ConnectionId,
    #[allow(dead_code)] // Will be used for routing and address resolution in future phases
    addr: String,
    receive_buffer: VecDeque<u8>,
    /// Paired connection for bidirectional communication
    paired_connection: Option<ConnectionId>,
}

/// Internal listener state for simulation
#[derive(Debug)]
struct ListenerState {
    #[allow(dead_code)] // Will be used for listener management in future phases
    id: ListenerId,
    #[allow(dead_code)] // Will be used for address binding in future phases
    addr: String,
    #[allow(dead_code)] // Will be used for connection queuing in future phases
    pending_connections: VecDeque<ConnectionId>,
}

#[derive(Debug)]
struct SimInner {
    current_time: Duration,
    event_queue: EventQueue,
    next_sequence: u64,

    // Phase 2b network simulation state
    next_connection_id: u64,
    next_listener_id: u64,

    // Phase 2c network configuration
    network_config: NetworkConfiguration,

    // Network state registries
    connections: HashMap<ConnectionId, ConnectionState>,
    listeners: HashMap<ListenerId, ListenerState>,

    // Waker storage for async coordination (simplified for Phase 2b)
    #[allow(dead_code)] // Will be used for proper async coordination in future phases
    connection_wakers: HashMap<ConnectionId, Waker>,
    #[allow(dead_code)] // Will be used for proper async coordination in future phases
    listener_wakers: HashMap<ListenerId, Waker>,
    #[allow(dead_code)] // Will be used for proper async coordination in future phases
    read_wakers: HashMap<ConnectionId, Waker>,

    // Phase 2d: Task management for sleep functionality
    next_task_id: u64,
    awakened_tasks: HashSet<u64>,
    task_wakers: HashMap<u64, Waker>,

    // Phase 2e: Pending connection pairs for bidirectional communication
    pending_connections: HashMap<String, ConnectionId>,
}

impl SimInner {
    fn new() -> Self {
        Self {
            current_time: Duration::ZERO, // Starts at Duration::ZERO (0 unix time)
            event_queue: EventQueue::new(),
            next_sequence: 0,

            next_connection_id: 0,
            next_listener_id: 0,

            // Default network configuration
            network_config: NetworkConfiguration::default(),

            connections: HashMap::new(),
            listeners: HashMap::new(),

            connection_wakers: HashMap::new(),
            listener_wakers: HashMap::new(),
            read_wakers: HashMap::new(),

            // Phase 2d: Initialize task management
            next_task_id: 0,
            awakened_tasks: HashSet::new(),
            task_wakers: HashMap::new(),

            // Phase 2e: Initialize pending connections
            pending_connections: HashMap::new(),
        }
    }

    fn new_with_config(network_config: NetworkConfiguration) -> Self {
        let mut inner = Self::new();
        inner.network_config = network_config;
        inner
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
    /// Creates a new simulation world with default network configuration.
    ///
    /// Uses default seed (0) for reproducible testing. For custom seeds,
    /// use [`SimWorld::new_with_seed`].
    pub fn new() -> Self {
        // Initialize with default seed for deterministic behavior
        reset_sim_rng();
        set_sim_seed(0);

        Self {
            inner: Rc::new(RefCell::new(SimInner::new())),
        }
    }

    /// Creates a new simulation world with a specific seed for deterministic randomness.
    ///
    /// This method ensures clean thread-local RNG state by resetting before
    /// setting the seed, making it safe for consecutive simulations on the same thread.
    ///
    /// # Parameters
    ///
    /// * `seed` - The seed value for deterministic randomness
    ///
    /// # Example
    ///
    /// ```rust
    /// use moonpool_simulation::SimWorld;
    ///
    /// // Create simulation with specific seed for reproducible behavior
    /// let mut sim = SimWorld::new_with_seed(42);
    /// // All random operations will be deterministic based on seed 42
    /// ```
    pub fn new_with_seed(seed: u64) -> Self {
        reset_sim_rng();
        set_sim_seed(seed);

        Self {
            inner: Rc::new(RefCell::new(SimInner::new())),
        }
    }

    /// Creates a new simulation world with custom network configuration.
    pub fn new_with_network_config(network_config: NetworkConfiguration) -> Self {
        // Initialize with default seed for deterministic behavior
        reset_sim_rng();
        set_sim_seed(0);

        Self {
            inner: Rc::new(RefCell::new(SimInner::new_with_config(network_config))),
        }
    }

    /// Creates a new simulation world with both custom network configuration and seed.
    ///
    /// # Parameters
    ///
    /// * `network_config` - Network configuration for latency and fault simulation
    /// * `seed` - The seed value for deterministic randomness
    ///
    /// # Example
    ///
    /// ```rust
    /// use moonpool_simulation::{SimWorld, NetworkConfiguration};
    ///
    /// let config = NetworkConfiguration::default();
    /// let mut sim = SimWorld::new_with_network_config_and_seed(config, 42);
    /// ```
    pub fn new_with_network_config_and_seed(
        network_config: NetworkConfiguration,
        seed: u64,
    ) -> Self {
        reset_sim_rng();
        set_sim_seed(seed);

        Self {
            inner: Rc::new(RefCell::new(SimInner::new_with_config(network_config))),
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

            // Process the event with the mutable reference
            Self::process_event_with_inner(&mut inner, scheduled_event.into_event());

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

    /// Create a network provider for this simulation
    pub fn network_provider(&self) -> SimNetworkProvider {
        SimNetworkProvider::new(self.downgrade())
    }

    /// Access network configuration for latency calculations using thread-local RNG.
    ///
    /// This method provides access to the network configuration for calculating
    /// latencies and other network parameters. Random values should be generated
    /// using the thread-local RNG functions like `sim_random()`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use moonpool_simulation::{SimWorld, sim_random_range};
    /// use std::time::Duration;
    ///
    /// let sim = SimWorld::new();
    /// let delay = sim.with_network_config(|config| {
    ///     // Use thread-local RNG for random delay
    ///     let base_latency = config.latency.connect_latency.base;
    ///     let jitter_ms = sim_random_range(0..100);
    ///     base_latency + Duration::from_millis(jitter_ms)
    /// });
    /// ```
    pub fn with_network_config<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&NetworkConfiguration) -> R,
    {
        let inner = self.inner.borrow();
        f(&inner.network_config)
    }

    /// Create a listener in the simulation (used by SimNetworkProvider)
    pub(crate) fn create_listener(&self, addr: String) -> SimulationResult<ListenerId> {
        let mut inner = self.inner.borrow_mut();
        let listener_id = ListenerId(inner.next_listener_id);
        inner.next_listener_id += 1;

        inner.listeners.insert(
            listener_id,
            ListenerState {
                id: listener_id,
                addr,
                pending_connections: VecDeque::new(),
            },
        );

        Ok(listener_id)
    }

    /// Read data from connection's receive buffer (used by SimTcpStream)
    pub(crate) fn read_from_connection(
        &self,
        connection_id: ConnectionId,
        buf: &mut [u8],
    ) -> SimulationResult<usize> {
        let mut inner = self.inner.borrow_mut();

        if let Some(connection) = inner.connections.get_mut(&connection_id) {
            let mut bytes_read = 0;
            while bytes_read < buf.len() && !connection.receive_buffer.is_empty() {
                if let Some(byte) = connection.receive_buffer.pop_front() {
                    buf[bytes_read] = byte;
                    bytes_read += 1;
                }
            }
            Ok(bytes_read)
        } else {
            Err(SimulationError::InvalidState(
                "connection not found".to_string(),
            ))
        }
    }

    /// Write data to connection's receive buffer (used by SimTcpStream write operations)
    pub(crate) fn write_to_connection(
        &self,
        connection_id: ConnectionId,
        data: &[u8],
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();

        if let Some(connection) = inner.connections.get_mut(&connection_id) {
            for &byte in data {
                connection.receive_buffer.push_back(byte);
            }
            Ok(())
        } else {
            Err(SimulationError::InvalidState(
                "connection not found".to_string(),
            ))
        }
    }

    /// Create a connection pair for bidirectional communication
    pub(crate) fn create_connection_pair(
        &self,
        client_addr: String,
        server_addr: String,
    ) -> SimulationResult<(ConnectionId, ConnectionId)> {
        let mut inner = self.inner.borrow_mut();

        let client_id = ConnectionId(inner.next_connection_id);
        inner.next_connection_id += 1;

        let server_id = ConnectionId(inner.next_connection_id);
        inner.next_connection_id += 1;

        // Create paired connections
        inner.connections.insert(
            client_id,
            ConnectionState {
                id: client_id,
                addr: client_addr,
                receive_buffer: VecDeque::new(),
                paired_connection: Some(server_id),
            },
        );

        inner.connections.insert(
            server_id,
            ConnectionState {
                id: server_id,
                addr: server_addr,
                receive_buffer: VecDeque::new(),
                paired_connection: Some(client_id),
            },
        );

        Ok((client_id, server_id))
    }

    /// Register a waker for read operations
    pub(crate) fn register_read_waker(
        &self,
        connection_id: ConnectionId,
        waker: Waker,
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.read_wakers.insert(connection_id, waker);
        Ok(())
    }

    /// Store a pending connection for later accept() call
    pub(crate) fn store_pending_connection(
        &self,
        addr: &str,
        connection_id: ConnectionId,
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner
            .pending_connections
            .insert(addr.to_string(), connection_id);
        Ok(())
    }

    /// Get a pending connection for accept() call
    pub(crate) fn get_pending_connection(
        &self,
        addr: &str,
    ) -> SimulationResult<Option<ConnectionId>> {
        let mut inner = self.inner.borrow_mut();
        Ok(inner.pending_connections.remove(addr))
    }

    /// Sleep for the specified duration in simulation time.
    ///
    /// Returns a future that will complete when the simulation time has advanced
    /// by the specified duration. This integrates with the event system by
    /// scheduling a Wake event and coordinating with the async runtime.
    ///
    /// # Example
    ///
    /// ```rust
    /// use moonpool_simulation::SimWorld;
    /// use std::time::Duration;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut sim = SimWorld::new();
    ///
    /// // Sleep for 100ms in simulation time
    /// sim.sleep(Duration::from_millis(100)).await?;
    ///
    /// // Advance simulation until the sleep completes
    /// sim.run_until_empty();
    ///
    /// assert_eq!(sim.current_time(), Duration::from_millis(100));
    /// # Ok(())
    /// # }
    /// ```
    pub fn sleep(&self, duration: Duration) -> SleepFuture {
        let task_id = self.generate_task_id();

        // Schedule a wake event for this task
        self.schedule_event(Event::Wake { task_id }, duration);

        // Return a future that will be woken when the event is processed
        SleepFuture::new(self.downgrade(), task_id)
    }

    /// Generate a unique task ID for sleep operations.
    fn generate_task_id(&self) -> u64 {
        let mut inner = self.inner.borrow_mut();
        let task_id = inner.next_task_id;
        inner.next_task_id += 1;
        task_id
    }

    /// Check if a task has been awakened.
    ///
    /// This is used internally by SleepFuture to determine if its corresponding
    /// Wake event has been processed.
    pub(crate) fn is_task_awake(&self, task_id: u64) -> SimulationResult<bool> {
        let inner = self.inner.borrow();
        Ok(inner.awakened_tasks.contains(&task_id))
    }

    /// Register a waker for a task.
    ///
    /// This is used internally by SleepFuture to register a waker that should
    /// be called when the task's Wake event is processed.
    pub(crate) fn register_task_waker(&self, task_id: u64, waker: Waker) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.task_wakers.insert(task_id, waker);
        Ok(())
    }

    fn process_event_with_inner(inner: &mut SimInner, event: Event) {
        // Process different event types
        match event {
            Event::Wake { task_id } => {
                // Phase 2d: Real task waking implementation

                // Mark this task as awakened
                inner.awakened_tasks.insert(task_id);

                // Wake the future that was sleeping
                if let Some(waker) = inner.task_wakers.remove(&task_id) {
                    waker.wake();
                }
            }
            Event::BindComplete { listener_id: _ } => {
                // Network bind completed - forward to network module for handling
                // For Phase 2c, this is just acknowledgment
                // In Phase 2d, this will wake futures and update state
            }
            Event::ConnectionReady { connection_id: _ } => {
                // Connection establishment completed - forward to network module for handling
                // For Phase 2c, this is just acknowledgment
                // In Phase 2d, this will wake futures and update state
            }
            Event::DataDelivery {
                connection_id,
                data,
            } => {
                // Data delivery completed - deliver data to paired connection's receive buffer
                let connection_id = ConnectionId(connection_id);

                // Find the paired connection
                let paired_id = inner
                    .connections
                    .get(&connection_id)
                    .and_then(|conn| conn.paired_connection);

                if let Some(paired_id) = paired_id {
                    // Write data to paired connection's receive buffer
                    if let Some(paired_conn) = inner.connections.get_mut(&paired_id) {
                        for &byte in &data {
                            paired_conn.receive_buffer.push_back(byte);
                        }

                        // Wake any futures waiting to read from paired connection
                        if let Some(waker) = inner.read_wakers.remove(&paired_id) {
                            waker.wake();
                        }
                    }
                }
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

    /// Access network configuration for latency calculations using thread-local RNG.
    ///
    /// Returns `Err(SimulationError::SimulationShutdown)` if the simulation
    /// has been dropped.
    pub fn with_network_config<F, R>(&self, f: F) -> SimulationResult<R>
    where
        F: FnOnce(&NetworkConfiguration) -> R,
    {
        let sim = self.upgrade()?;
        Ok(sim.with_network_config(f))
    }

    /// Read data from connection's receive buffer
    ///
    /// Returns `Err(SimulationError::SimulationShutdown)` if the simulation
    /// has been dropped.
    pub fn read_from_connection(
        &self,
        connection_id: ConnectionId,
        buf: &mut [u8],
    ) -> SimulationResult<usize> {
        let sim = self.upgrade()?;
        sim.read_from_connection(connection_id, buf)
    }

    /// Write data to connection's receive buffer
    ///
    /// Returns `Err(SimulationError::SimulationShutdown)` if the simulation
    /// has been dropped.
    pub fn write_to_connection(
        &self,
        connection_id: ConnectionId,
        data: &[u8],
    ) -> SimulationResult<()> {
        let sim = self.upgrade()?;
        sim.write_to_connection(connection_id, data)
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
