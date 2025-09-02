use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::{Rc, Weak},
    task::Waker,
    time::Duration,
};
use tracing::instrument;

use crate::{
    assertions::reset_assertion_results,
    error::{SimulationError, SimulationResult},
    events::{Event, EventQueue, ScheduledEvent},
    network::{
        NetworkConfiguration,
        sim::{ConnectionId, ListenerId, SimNetworkProvider},
    },
    network_state::{ClogState, ConnectionState, CutState, ListenerState, NetworkState},
    rng::{reset_sim_rng, set_sim_seed, sim_random},
    sleep::SleepFuture,
};
use std::collections::VecDeque;

/// Waker management for async coordination.
#[derive(Debug, Default)]
struct WakerRegistry {
    #[allow(dead_code)] // Will be used for connection coordination in future phases
    connection_wakers: HashMap<ConnectionId, Waker>,
    listener_wakers: HashMap<ListenerId, Waker>,
    read_wakers: HashMap<ConnectionId, Waker>,
    task_wakers: HashMap<u64, Waker>,
    clog_wakers: HashMap<ConnectionId, Vec<Waker>>,
    cut_wakers: HashMap<ConnectionId, Vec<Waker>>,
}

#[derive(Debug)]
struct SimInner {
    current_time: Duration,
    event_queue: EventQueue,
    next_sequence: u64,

    // Network management
    network: NetworkState,

    // Async coordination
    wakers: WakerRegistry,

    // Task management for sleep functionality
    next_task_id: u64,
    awakened_tasks: HashSet<u64>,

    // Event processing metrics
    events_processed: u64,
}

impl SimInner {
    fn new() -> Self {
        Self {
            current_time: Duration::ZERO,
            event_queue: EventQueue::new(),
            next_sequence: 0,
            network: NetworkState::new(NetworkConfiguration::default()),
            wakers: WakerRegistry::default(),
            next_task_id: 0,
            awakened_tasks: HashSet::new(),
            events_processed: 0,
        }
    }

    fn new_with_config(network_config: NetworkConfiguration) -> Self {
        Self {
            current_time: Duration::ZERO,
            event_queue: EventQueue::new(),
            next_sequence: 0,
            network: NetworkState::new(network_config),
            wakers: WakerRegistry::default(),
            next_task_id: 0,
            awakened_tasks: HashSet::new(),
            events_processed: 0,
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
    /// Creates a new simulation world with default network configuration.
    ///
    /// Uses default seed (0) for reproducible testing. For custom seeds,
    /// use [`SimWorld::new_with_seed`].
    pub fn new() -> Self {
        // Initialize with default seed for deterministic behavior
        reset_sim_rng();
        set_sim_seed(0);
        reset_assertion_results();

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
        reset_assertion_results();

        Self {
            inner: Rc::new(RefCell::new(SimInner::new())),
        }
    }

    /// Creates a new simulation world with custom network configuration.
    pub fn new_with_network_config(network_config: NetworkConfiguration) -> Self {
        // Initialize with default seed for deterministic behavior
        reset_sim_rng();
        set_sim_seed(0);
        reset_assertion_results();

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
        reset_assertion_results();

        Self {
            inner: Rc::new(RefCell::new(SimInner::new_with_config(network_config))),
        }
    }

    /// Processes the next scheduled event and advances time.
    ///
    /// Returns `true` if more events are available for processing,
    /// `false` if this was the last event or if no events are available.
    #[instrument(skip(self))]
    pub fn step(&mut self) -> bool {
        let mut inner = self.inner.borrow_mut();

        if let Some(scheduled_event) = inner.event_queue.pop_earliest() {
            // Advance logical time to event timestamp
            inner.current_time = scheduled_event.time();

            // Phase 7: Clear expired clogs after time advancement
            Self::clear_expired_clogs_with_inner(&mut inner);

            // Phase 7b: Restore cut connections after time advancement
            Self::restore_cut_connections_with_inner(&mut inner);

            // Phase 7b: Randomly cut connections based on probability
            Self::randomly_cut_connections_with_inner(&mut inner);

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
    #[instrument(skip(self))]
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
    #[instrument(skip(self))]
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

    /// Create a time provider for this simulation
    pub fn time_provider(&self) -> crate::time::SimTimeProvider {
        crate::time::SimTimeProvider::new(self.downgrade())
    }

    /// Create a task provider for this simulation
    pub fn task_provider(&self) -> crate::task::tokio_provider::TokioTaskProvider {
        crate::task::tokio_provider::TokioTaskProvider
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
        f(&inner.network.config)
    }

    /// Create a listener in the simulation (used by SimNetworkProvider)
    pub(crate) fn create_listener(&self, addr: String) -> SimulationResult<ListenerId> {
        let mut inner = self.inner.borrow_mut();
        let listener_id = ListenerId(inner.network.next_listener_id);
        inner.network.next_listener_id += 1;

        inner.network.listeners.insert(
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

        if let Some(connection) = inner.network.connections.get_mut(&connection_id) {
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

        if let Some(connection) = inner.network.connections.get_mut(&connection_id) {
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

    /// Buffer data for ordered sending on a TCP connection.
    ///
    /// This method implements the core TCP ordering guarantee by ensuring that all
    /// write operations on a single connection are processed in FIFO (First-In-First-Out) order.
    ///
    /// ## TCP Ordering Problem Solved
    ///
    /// The original implementation gave each `write_all()` call an independent random network delay,
    /// which could cause messages to arrive out of order:
    ///
    /// ```text
    /// BROKEN: Independent delays per write
    /// write("PING-0") -> 50ms delay -> arrives second
    /// write("PING-1") -> 20ms delay -> arrives first  ❌ WRONG ORDER
    /// ```
    ///
    /// This method fixes the problem by buffering all writes and processing them sequentially:
    ///
    /// ```text
    /// FIXED: Sequential processing with buffering
    /// write("PING-0") -> buffer[0] -> process -> 50ms -> arrives first
    /// write("PING-1") -> buffer[1] -> wait    -> 1ns  -> arrives second ✅ CORRECT ORDER
    /// ```
    ///
    /// ## Implementation Strategy
    ///
    /// 1. **Buffer the data**: Add the message to the connection's `send_buffer`
    /// 2. **Start processing if idle**: If no send operation is in progress, schedule a `ProcessSendBuffer` event
    /// 3. **Sequential processing**: The event handler processes messages one by one, maintaining order
    /// 4. **Network delay**: Only the final message in a burst gets the full network delay
    ///
    /// ## Arguments
    ///
    /// * `connection_id` - The connection to send data on
    /// * `data` - The message data to send (typically from a `write_all()` call)
    ///
    /// ## Returns
    ///
    /// * `Ok(())` - Data successfully buffered for sending
    /// * `Err(SimulationError)` - Connection not found or simulation error
    ///
    /// ## Usage Pattern
    ///
    /// This method is called by `SimTcpStream::poll_write()` for every write operation:
    ///
    /// ```rust
    /// // Application code
    /// stream.write_all(b"message1").await?;  // Calls buffer_send("message1")
    /// stream.write_all(b"message2").await?;  // Calls buffer_send("message2")
    ///
    /// // Results in ordered delivery: message1 then message2
    /// ```
    ///
    /// ## Event Flow
    ///
    /// 1. `buffer_send()` adds data to `send_buffer`
    /// 2. If `!send_in_progress`, schedules `ProcessSendBuffer` event immediately  
    /// 3. `ProcessSendBuffer` handler dequeues and sends one message
    /// 4. If more messages remain, schedules next `ProcessSendBuffer` event
    /// 5. Continues until `send_buffer` is empty
    ///
    /// This ensures that even rapid successive writes are processed in order.
    pub(crate) fn buffer_send(
        &self,
        connection_id: ConnectionId,
        data: Vec<u8>,
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();

        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            // Always add data to send buffer for TCP ordering
            conn.send_buffer.push_back(data);

            // If sender is not already active, start processing the buffer
            if !conn.send_in_progress {
                conn.send_in_progress = true;

                // Schedule immediate processing of the buffer - add directly to queue to avoid borrowing conflict
                let scheduled_time = inner.current_time + std::time::Duration::ZERO;
                let sequence = inner.next_sequence;
                inner.next_sequence += 1;
                let scheduled_event = ScheduledEvent::new(
                    scheduled_time,
                    Event::ProcessSendBuffer {
                        connection_id: connection_id.0,
                    },
                    sequence,
                );
                inner.event_queue.schedule(scheduled_event);
            }
            // If sender is already active, the new data will be processed when the current buffer is processed

            Ok(())
        } else {
            Err(SimulationError::InvalidState(
                "connection not found".to_string(),
            ))
        }
    }

    /// Create a bidirectional TCP connection pair for client-server communication.
    ///
    /// This method establishes the foundation of TCP simulation by creating two linked
    /// `ConnectionState` instances that represent both ends of a TCP connection.
    ///
    /// ## Connection Pair Architecture
    ///
    /// ```text
    /// ┌─────────────────────────────────────────────────────────────┐
    /// │                    TCP Connection Pair                      │
    /// │                                                             │
    /// │  Client Side (ID: N)              Server Side (ID: N+1)     │
    /// │  ┌─────────────────────┐          ┌─────────────────────┐   │
    /// │  │ addr: "client-addr" │          │ addr: server_addr   │   │
    /// │  │ paired_connection: │◄─────────►│ paired_connection:  │   │
    /// │  │   Some(N+1)        │          │   Some(N)           │   │
    /// │  │                    │          │                     │   │
    /// │  │ send_buffer: []    │   ┌──┐   │ receive_buffer: []  │   │
    /// │  │ receive_buffer: [] │   │  │   │ send_buffer: []     │   │
    /// │  └─────────────────────┘   └──┘   └─────────────────────┘   │
    /// └─────────────────────────────────────────────────────────────┘
    /// ```
    ///
    /// ## Connection Pairing Logic
    ///
    /// Each connection knows about its counterpart via `paired_connection`:
    /// - **Client connection** has `paired_connection = Some(server_id)`
    /// - **Server connection** has `paired_connection = Some(client_id)`
    ///
    /// This enables the data flow:
    /// 1. Data written to client's `send_buffer` → delivered to server's `receive_buffer`
    /// 2. Data written to server's `send_buffer` → delivered to client's `receive_buffer`
    ///
    /// ## Usage in Network Provider
    ///
    /// This method is called by `SimNetworkProvider::connect()`:
    ///
    /// ```rust
    /// // Client initiates connection
    /// let stream = provider.connect("10.0.0.1:8080").await?;
    ///
    /// // Internally calls:
    /// let (client_id, server_id) = sim.create_connection_pair(
    ///     "client-addr".to_string(),
    ///     "10.0.0.1:8080".to_string()
    /// )?;
    ///
    /// // Client gets stream with client_id
    /// // Server gets stream with server_id via accept()
    /// ```
    ///
    /// ## Connection ID Management
    ///
    /// Connection IDs are assigned sequentially:
    /// - **Client connection**: Uses current `next_connection_id` (even numbers: 0, 2, 4...)
    /// - **Server connection**: Uses `next_connection_id + 1` (odd numbers: 1, 3, 5...)
    /// - Counter increments by 2 to reserve both IDs
    ///
    /// This pairing convention makes debugging easier and ensures unique IDs.
    ///
    /// ## Arguments
    ///
    /// * `client_addr` - Address string for the client side (typically generated)
    /// * `server_addr` - Address string for the server side (the bind address)
    ///
    /// ## Returns
    ///
    /// * `Ok((client_id, server_id))` - Connection IDs for both ends of the pair
    /// * `Err(SimulationError)` - If connection creation fails
    ///
    /// ## Connection Lifecycle
    ///
    /// 1. **Creation**: Both connections start with empty buffers and `send_in_progress = false`
    /// 2. **Active Use**: Applications write/read data via `SimTcpStream` operations
    /// 3. **Data Flow**: Writes go to `send_buffer`, reads come from `receive_buffer`
    /// 4. **Event Processing**: `ProcessSendBuffer` and `DataDelivery` events handle data transfer
    /// 5. **Cleanup**: Connections remain until simulation ends (no explicit close yet)
    pub(crate) fn create_connection_pair(
        &self,
        client_addr: String,
        server_addr: String,
    ) -> SimulationResult<(ConnectionId, ConnectionId)> {
        let mut inner = self.inner.borrow_mut();

        let client_id = ConnectionId(inner.network.next_connection_id);
        inner.network.next_connection_id += 1;

        let server_id = ConnectionId(inner.network.next_connection_id);
        inner.network.next_connection_id += 1;

        // Capture current time to avoid borrow conflicts
        let current_time = inner.current_time;

        // Create paired connections
        inner.network.connections.insert(
            client_id,
            ConnectionState {
                id: client_id,
                addr: client_addr,
                receive_buffer: VecDeque::new(),
                paired_connection: Some(server_id),
                send_buffer: VecDeque::new(),
                send_in_progress: false,
                next_send_time: current_time,
            },
        );

        inner.network.connections.insert(
            server_id,
            ConnectionState {
                id: server_id,
                addr: server_addr,
                receive_buffer: VecDeque::new(),
                paired_connection: Some(client_id),
                send_buffer: VecDeque::new(),
                send_in_progress: false,
                next_send_time: current_time,
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
        inner.wakers.read_wakers.insert(connection_id, waker);
        Ok(())
    }

    /// Register a waker for accept operations
    pub(crate) fn register_accept_waker(&self, addr: &str, waker: Waker) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        // For simplicity, we'll use addr hash as listener ID for waker storage
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        addr.hash(&mut hasher);
        let listener_key = ListenerId(hasher.finish());

        inner.wakers.listener_wakers.insert(listener_key, waker);
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
            .network
            .pending_connections
            .insert(addr.to_string(), connection_id);

        // Wake any accept() calls waiting for this connection
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        addr.hash(&mut hasher);
        let listener_key = ListenerId(hasher.finish());

        if let Some(waker) = inner.wakers.listener_wakers.remove(&listener_key) {
            waker.wake();
        }

        Ok(())
    }

    /// Get a pending connection for accept() call
    pub(crate) fn get_pending_connection(
        &self,
        addr: &str,
    ) -> SimulationResult<Option<ConnectionId>> {
        let mut inner = self.inner.borrow_mut();
        Ok(inner.network.pending_connections.remove(addr))
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
    #[instrument(skip(self))]
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

    /// Wake all tasks associated with a connection
    fn wake_all(wakers: &mut HashMap<ConnectionId, Vec<Waker>>, connection_id: ConnectionId) {
        if let Some(waker_list) = wakers.remove(&connection_id) {
            for waker in waker_list {
                waker.wake();
            }
        }
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
        inner.wakers.task_wakers.insert(task_id, waker);
        Ok(())
    }

    /// Static event processor for simulation events with comprehensive TCP simulation support.
    ///
    /// This method is the heart of the simulation engine, processing all types of events
    /// that drive the deterministic simulation forward. It's implemented as a static method
    /// to avoid borrowing conflicts when event processing needs to modify simulation state.
    ///
    /// ## Event Processing Architecture
    ///
    /// ```text
    /// Event Queue                Event Processor              Connection State
    /// ─────────────             ─────────────────            ──────────────────
    ///
    /// [TaskWake]           ──► process_event_with_inner ──► awakened_tasks.insert()
    /// [DataDelivery]       ──►        │                 ──► connection.receive_buffer.push()
    /// [ProcessSendBuffer]  ──►        │                 ──► connection.send_buffer.pop()
    /// [ConnectionReady]    ──►        │                 ──► // Future connection events
    /// ```
    ///
    /// ## TCP-Specific Event Handling
    ///
    /// ### ProcessSendBuffer Event
    /// Core of the TCP ordering fix - ensures FIFO message delivery:
    /// 1. **Message Dequeue**: Removes next message from connection's send_buffer
    /// 2. **Delay Strategy**:
    ///    - Last message in buffer: Full network delay (realistic latency)
    ///    - More messages queued: Minimal delay (1ns) for immediate processing
    /// 3. **Delivery Scheduling**: Creates DataDelivery event for paired connection
    /// 4. **Continuation**: Schedules next ProcessSendBuffer if buffer not empty
    ///
    /// ### DataDelivery Event
    /// Handles reliable data transfer between connection pairs:
    /// 1. **Target Resolution**: Finds destination connection by ID
    /// 2. **Data Transfer**: Appends data to connection's receive_buffer
    /// 3. **Notification**: Wakes any pending read operations on that connection
    /// 4. **Logging**: Comprehensive tracing for debugging network behavior
    ///
    /// ## Event Processing Guarantees
    ///
    /// - **Atomicity**: Each event is processed completely before the next
    /// - **Ordering**: Events are processed by scheduled time, then sequence number
    /// - **Isolation**: No event can interfere with another's processing
    /// - **Determinism**: Same events + same order = identical simulation results
    ///
    /// ## Error Handling Strategy
    ///
    /// The method is designed to be robust against various error conditions:
    /// - **Missing connections**: Log warnings but continue processing
    /// - **Invalid wakers**: Handle gracefully without simulation failure  
    /// - **Empty buffers**: Treat as normal operation completion
    ///
    /// ## Performance Characteristics
    ///
    /// - **Time Complexity**: O(1) for most events, O(log n) for queue operations
    /// - **Memory Usage**: Minimal - operates on existing connection state
    /// - **Event Throughput**: Limited by connection buffer operations, not event count
    ///
    /// ## Integration with AsyncRead/AsyncWrite
    ///
    /// This event processor directly supports the async stream operations:
    /// - **Write operations** → buffer_send() → ProcessSendBuffer events
    /// - **Read operations** → register_read_waker() → DataDelivery wake-ups
    /// - **Connection setup** → ConnectionReady events (future enhancement)
    ///
    /// The static design prevents the common async borrowing issue where event
    /// processing needs to modify simulation state while async operations hold borrows.
    /// Clear expired clogs and wake pending tasks (helper for use with SimInner)
    fn clear_expired_clogs_with_inner(inner: &mut SimInner) {
        let now = inner.current_time;
        let expired: Vec<ConnectionId> = inner
            .network
            .connection_clogs
            .iter()
            .filter_map(|(id, state)| (now >= state.expires_at).then_some(*id))
            .collect();

        for id in expired {
            inner.network.connection_clogs.remove(&id);
            Self::wake_all(&mut inner.wakers.clog_wakers, id);
        }
    }

    #[instrument(skip(inner))]
    fn process_event_with_inner(inner: &mut SimInner, event: Event) {
        // Increment event processing counter for metrics
        inner.events_processed += 1;

        // Process different event types
        match event {
            Event::Wake { task_id } => {
                // Phase 2d: Real task waking implementation

                // Mark this task as awakened
                inner.awakened_tasks.insert(task_id);

                // Wake the future that was sleeping
                if let Some(waker) = inner.wakers.task_wakers.remove(&task_id) {
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
                // **DataDelivery Event Handler**: Core TCP data transfer implementation
                //
                // This event handler completes the final step of TCP message delivery by
                // transferring data from the sender's send_buffer to the receiver's receive_buffer.
                // It's the endpoint of the TCP ordering pipeline that maintains reliable delivery.
                //
                // Event Flow Context:
                // 1. Application calls stream.write_all(data)  -> buffer_send(data)
                // 2. buffer_send() adds to send_buffer        -> ProcessSendBuffer event scheduled
                // 3. ProcessSendBuffer dequeues data          -> DataDelivery event scheduled (THIS EVENT)
                // 4. DataDelivery writes to receive_buffer   -> Application stream.read() succeeds
                //
                // Key Responsibilities:
                // - **Data Transfer**: Move bytes from network simulation to connection buffer
                // - **Async Notification**: Wake any read operations waiting for this data
                // - **Reliability**: Ensure no data is lost in transfer
                // - **Ordering**: Maintain FIFO delivery within the connection
                //
                // Critical Fix (Phase 6): This handler now delivers data to the CORRECT connection
                // - Previous bug: delivered to paired_connection (wrong direction!)
                // - Current fix: delivers directly to specified connection_id (correct!)
                //
                // Connection ID Routing:
                // - Client writes to server: Client ProcessSendBuffer -> Server DataDelivery
                // - Server writes to client: Server ProcessSendBuffer -> Client DataDelivery
                // - Each DataDelivery specifies the TARGET connection to receive the data
                let data_preview = String::from_utf8_lossy(&data[..std::cmp::min(data.len(), 20)]);
                tracing::info!(
                    "Event::DataDelivery processing delivery of {} bytes: '{}' to connection {}",
                    data.len(),
                    data_preview,
                    connection_id
                );

                let connection_id = ConnectionId(connection_id);

                // Write data directly to the specified connection's receive buffer
                if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
                    for &byte in &data {
                        conn.receive_buffer.push_back(byte);
                    }

                    // Wake any futures waiting to read from this connection
                    if let Some(waker) = inner.wakers.read_wakers.remove(&connection_id) {
                        tracing::info!(
                            "DataDelivery waking up read waker for connection_id={}",
                            connection_id.0
                        );
                        waker.wake();
                    } else {
                        tracing::info!(
                            "DataDelivery no waker found for connection_id={}",
                            connection_id.0
                        );
                    }
                }
            }
            // Process the next message from a connection's send buffer.
            //
            // This event handler is the core of the TCP ordering fix. It ensures that
            // messages are sent in FIFO order by processing the send buffer sequentially.
            //
            // Event Processing Flow:
            // 1. Dequeue Message: Remove the next message from send_buffer
            // 2. Apply Delay Logic:
            //    - If more messages remain: minimal delay (1ns) for immediate processing
            //    - If this is the last message: full network delay simulation
            // 3. Schedule Delivery: Create DataDelivery event to paired connection
            // 4. Continue Processing: If more messages remain, schedule next ProcessSendBuffer
            // 5. Mark Idle: If buffer empty, set send_in_progress = false
            //
            // TCP Ordering Guarantee:
            // The key insight is that within a connection, we want FIFO ordering but still
            // need realistic network delays. The solution:
            //
            // Message Flow Timeline:
            // T=0ms:  buffer_send("A") -> send_buffer = ["A"]          -> ProcessSendBuffer scheduled
            // T=1ms:  buffer_send("B") -> send_buffer = ["A", "B"]     -> (ProcessSendBuffer already active)
            // T=2ms:  buffer_send("C") -> send_buffer = ["A","B","C"]  -> (ProcessSendBuffer already active)
            //
            // T=5ms:  ProcessSendBuffer -> pop "A", 2 msgs remaining   -> DataDelivery("A") @ T=6ms (1ns delay)
            // T=6ms:  ProcessSendBuffer -> pop "B", 1 msg remaining    -> DataDelivery("B") @ T=7ms (1ns delay)
            // T=7ms:  ProcessSendBuffer -> pop "C", 0 msgs remaining   -> DataDelivery("C") @ T=57ms (50ms delay)
            //
            // Result: A arrives at T=6ms, B arrives at T=7ms, C arrives at T=57ms ✅ ORDERED
            //
            // This preserves both ordering (A→B→C) and realistic network delays (C gets full latency).
            Event::ProcessSendBuffer { connection_id } => {
                let connection_id = ConnectionId(connection_id);

                if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
                    if let Some(data) = conn.send_buffer.pop_front() {
                        // For TCP ordering, we need to maintain connection-level delays
                        // Check if there are more messages AFTER popping the current one
                        let has_more_messages = !conn.send_buffer.is_empty();
                        let base_delay = if has_more_messages {
                            // More messages in buffer, minimal delay for immediate processing
                            std::time::Duration::from_nanos(1)
                        } else {
                            // This is the last message in buffer, apply network delay
                            inner.network.config.latency.write_latency.sample()
                        };

                        // Ensure TCP ordering: schedule at next available time for this connection
                        let earliest_time =
                            std::cmp::max(inner.current_time + base_delay, conn.next_send_time);
                        let actual_delay = earliest_time - inner.current_time;

                        // Update next available send time for this connection
                        conn.next_send_time = earliest_time + std::time::Duration::from_nanos(1);

                        tracing::info!(
                            "Event::ProcessSendBuffer processing {} bytes from connection {} with delay {:?}, has_more_messages={} (TCP ordering: earliest_time={:?})",
                            data.len(),
                            connection_id.0,
                            actual_delay,
                            has_more_messages,
                            earliest_time
                        );

                        // Schedule delivery to paired connection
                        if let Some(paired_id) = conn.paired_connection {
                            let scheduled_time = earliest_time;
                            let sequence = inner.next_sequence;
                            inner.next_sequence += 1;

                            let scheduled_event = ScheduledEvent::new(
                                scheduled_time,
                                Event::DataDelivery {
                                    connection_id: paired_id.0,
                                    data,
                                },
                                sequence,
                            );
                            inner.event_queue.schedule(scheduled_event);
                        }

                        // If more messages in buffer, schedule next processing immediately to maintain throughput
                        if !conn.send_buffer.is_empty() {
                            let scheduled_time = inner.current_time; // Process immediately
                            let sequence = inner.next_sequence;
                            inner.next_sequence += 1;

                            let scheduled_event = ScheduledEvent::new(
                                scheduled_time,
                                Event::ProcessSendBuffer {
                                    connection_id: connection_id.0,
                                },
                                sequence,
                            );
                            inner.event_queue.schedule(scheduled_event);
                        } else {
                            // Mark sender as no longer active
                            conn.send_in_progress = false;
                        }
                    } else {
                        // No more messages, mark sender as inactive
                        conn.send_in_progress = false;
                    }
                }
            }
            Event::ClogClear { connection_id } => {
                // Phase 7: Clear clog for the specified connection and wake any waiting tasks
                let connection_id = ConnectionId(connection_id);

                inner.network.connection_clogs.remove(&connection_id);
                if let Some(wakers) = inner.wakers.clog_wakers.remove(&connection_id) {
                    for waker in wakers {
                        waker.wake();
                    }
                }
            }
        }
    }

    /// Get current assertion results for all tracked assertions.
    ///
    /// Returns a snapshot of assertion statistics collected during this simulation.
    /// This provides access to both `always_assert!` and `sometimes_assert!` results
    /// for statistical analysis of distributed system properties.
    ///
    /// # Example
    ///
    /// ```rust
    /// use moonpool_simulation::{SimWorld, always_assert, sometimes_assert};
    /// use std::time::Duration;
    ///
    /// let sim = SimWorld::new_with_seed(42);
    ///
    /// // Example assertions (these would be in simulation logic)
    /// always_assert!(system_running, true, "System should be running");
    /// sometimes_assert!(fast_response, Duration::from_millis(50) < Duration::from_millis(100), "Responses should be fast");
    ///
    /// // Access results
    /// let results = sim.assertion_results();
    /// println!("System running: {:.2}% success", results.get("system_running").map_or(0.0, |s| s.success_rate()));
    /// println!("Fast response rate: {:.2}%", results.get("fast_response").map_or(0.0, |s| s.success_rate()));
    /// ```
    pub fn assertion_results(
        &self,
    ) -> std::collections::HashMap<String, crate::assertions::AssertionStats> {
        crate::assertions::get_assertion_results()
    }

    /// Reset assertion statistics to empty state.
    ///
    /// This should be called before each simulation run to ensure clean state
    /// between consecutive simulations. It is automatically called by
    /// `new_with_seed()` and related methods.
    ///
    /// # Example
    ///
    /// ```rust
    /// use moonpool_simulation::{SimWorld, sometimes_assert};
    ///
    /// let sim = SimWorld::new();
    /// sometimes_assert!(test_assertion, true, "Test assertion");
    /// assert_eq!(sim.assertion_results()["test_assertion"].total_checks, 1);
    ///
    /// sim.reset_assertion_results();
    /// assert!(sim.assertion_results().is_empty());
    /// ```
    pub fn reset_assertion_results(&self) {
        crate::assertions::reset_assertion_results();
    }

    /// Extract simulation metrics for reporting.
    ///
    /// Returns the current simulation metrics including simulated time,
    /// events processed, and custom metrics. This is useful for integration
    /// with the simulation reporting framework.
    ///
    /// # Example
    ///
    /// ```rust
    /// use moonpool_simulation::SimWorld;
    ///
    /// let mut sim = SimWorld::new();
    /// sim.run_until_empty();
    ///
    /// let metrics = sim.extract_metrics();
    /// println!("Simulated time: {:?}", metrics.simulated_time);
    /// println!("Events processed: {}", metrics.events_processed);
    /// ```
    pub fn extract_metrics(&self) -> crate::runner::SimulationMetrics {
        let inner = self.inner.borrow();

        crate::runner::SimulationMetrics {
            wall_time: std::time::Duration::ZERO, // This will be filled by the report builder
            simulated_time: inner.current_time,
            events_processed: inner.events_processed,
        }
    }

    // Phase 7: Simple write clogging methods

    /// Check if a write should be clogged based on probability
    pub fn should_clog_write(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        let config = &inner.network.config.clogging;

        // Skip if already clogged
        if let Some(clog_state) = inner.network.connection_clogs.get(&connection_id) {
            return inner.current_time < clog_state.expires_at;
        }

        // Check probability
        config.probability > 0.0 && sim_random::<f64>() < config.probability
    }

    /// Clog a connection's write operations
    pub fn clog_write(&self, connection_id: ConnectionId) {
        let mut inner = self.inner.borrow_mut();
        let config = &inner.network.config.clogging;

        let clog_duration = config.duration.sample();
        let expires_at = inner.current_time + clog_duration;
        inner
            .network
            .connection_clogs
            .insert(connection_id, ClogState { expires_at });

        // Schedule an event to clear this clog
        let clear_event = Event::ClogClear {
            connection_id: connection_id.0,
        };
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        inner
            .event_queue
            .schedule(ScheduledEvent::new(expires_at, clear_event, sequence));
    }

    /// Check if a connection's writes are currently clogged
    pub fn is_write_clogged(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();

        if let Some(clog_state) = inner.network.connection_clogs.get(&connection_id) {
            inner.current_time < clog_state.expires_at
        } else {
            false
        }
    }

    /// Register a waker for when clog clears
    pub fn register_clog_waker(&self, connection_id: ConnectionId, waker: Waker) {
        let mut inner = self.inner.borrow_mut();
        inner
            .wakers
            .clog_wakers
            .entry(connection_id)
            .or_default()
            .push(waker);
    }

    /// Clear expired clogs and wake pending tasks
    pub fn clear_expired_clogs(&self) {
        let mut inner = self.inner.borrow_mut();
        let now = inner.current_time;
        let expired: Vec<ConnectionId> = inner
            .network
            .connection_clogs
            .iter()
            .filter_map(|(id, state)| (now >= state.expires_at).then_some(*id))
            .collect();

        for id in expired {
            inner.network.connection_clogs.remove(&id);
            Self::wake_all(&mut inner.wakers.clog_wakers, id);
        }
    }

    // Phase 7b: Connection cutting methods

    /// Check if a connection should be cut based on probability and limits
    pub fn should_cut_connection(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        let config = &inner.network.config.cutting;

        // Skip if already cut
        if inner.network.cut_connections.contains_key(&connection_id) {
            return false;
        }

        // Check max cuts limit
        if let Some(max_cuts) = config.max_cuts_per_connection {
            // Count previous cuts for this connection
            let previous_cuts = inner
                .network
                .cut_connections
                .values()
                .filter(|cut_state| cut_state.connection_data.id == connection_id)
                .count() as u32;
            if previous_cuts >= max_cuts {
                return false;
            }
        }

        // Probability check
        sim_random::<f64>() < config.probability
    }

    /// Cut a connection and store it for later restoration
    pub fn cut_connection(&self, connection_id: ConnectionId) {
        let mut inner = self.inner.borrow_mut();
        let reconnect_delay = inner.network.config.cutting.reconnect_delay.clone();

        if let Some(connection_state) = inner.network.connections.remove(&connection_id) {
            let reconnect_delay_sample = reconnect_delay.sample();
            let reconnect_at = inner.current_time + reconnect_delay_sample;

            // Count existing cuts for this connection
            let cut_count = inner
                .network
                .cut_connections
                .values()
                .filter(|cut_state| cut_state.connection_data.id == connection_id)
                .count() as u32;

            let cut_state = CutState {
                connection_data: connection_state,
                reconnect_at,
                cut_count: cut_count + 1,
            };

            inner
                .network
                .cut_connections
                .insert(connection_id, cut_state);
        }
    }

    /// Check if a connection is currently cut
    pub fn is_connection_cut(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        inner.network.cut_connections.contains_key(&connection_id)
    }

    /// Register a waker for when connection is restored
    pub fn register_cut_waker(&self, connection_id: ConnectionId, waker: Waker) {
        let mut inner = self.inner.borrow_mut();
        inner
            .wakers
            .cut_wakers
            .entry(connection_id)
            .or_default()
            .push(waker);
    }

    /// Restore connections that are ready to be reconnected
    pub fn restore_cut_connections(&self) {
        let mut inner = self.inner.borrow_mut();
        Self::restore_cut_connections_with_inner(&mut inner);
    }

    /// Randomly cut connections based on configuration probability
    pub fn randomly_cut_connections(&self) {
        let connection_ids: Vec<ConnectionId> = {
            let inner = self.inner.borrow();
            inner.network.connections.keys().copied().collect()
        };

        for connection_id in connection_ids {
            if self.should_cut_connection(connection_id) {
                self.cut_connection(connection_id);
            }
        }
    }

    /// Helper method for use with SimInner - restore cut connections
    fn restore_cut_connections_with_inner(inner: &mut SimInner) {
        let now = inner.current_time;
        let ready_connections: Vec<ConnectionId> = inner
            .network
            .cut_connections
            .iter()
            .filter_map(|(id, state)| (now >= state.reconnect_at).then_some(*id))
            .collect();

        for connection_id in ready_connections {
            if let Some(cut_state) = inner.network.cut_connections.remove(&connection_id) {
                inner
                    .network
                    .connections
                    .insert(connection_id, cut_state.connection_data);
                Self::wake_all(&mut inner.wakers.cut_wakers, connection_id);
            }
        }
    }

    /// Helper method for use with SimInner - randomly cut connections
    fn randomly_cut_connections_with_inner(inner: &mut SimInner) {
        let cutting_config = inner.network.config.cutting.clone();

        // Skip if cutting is disabled
        if cutting_config.probability == 0.0 {
            return;
        }

        let connection_ids: Vec<ConnectionId> = inner.network.connections.keys().copied().collect();

        for connection_id in connection_ids {
            // Skip if already cut
            if inner.network.cut_connections.contains_key(&connection_id) {
                continue;
            }

            // Check max cuts limit
            if let Some(max_cuts) = cutting_config.max_cuts_per_connection {
                let previous_cuts = inner
                    .network
                    .cut_connections
                    .values()
                    .filter(|cut_state| cut_state.connection_data.id == connection_id)
                    .count() as u32;
                if previous_cuts >= max_cuts {
                    continue;
                }
            }

            // Probability check
            if sim_random::<f64>() < cutting_config.probability
                && let Some(connection_state) = inner.network.connections.remove(&connection_id)
            {
                let reconnect_delay = cutting_config.reconnect_delay.sample();
                let reconnect_at = inner.current_time + reconnect_delay;

                let cut_count = inner
                    .network
                    .cut_connections
                    .values()
                    .filter(|cut_state| cut_state.connection_data.id == connection_id)
                    .count() as u32;

                let cut_state = CutState {
                    connection_data: connection_state,
                    reconnect_at,
                    cut_count: cut_count + 1,
                };

                inner
                    .network
                    .cut_connections
                    .insert(connection_id, cut_state);
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

    /// Buffer data for ordered sending on a connection.
    ///
    /// This method implements TCP-like ordering by buffering the data and processing
    /// it through a FIFO queue to prevent message reordering due to random delays.
    ///
    /// Returns `Err(SimulationError::SimulationShutdown)` if the simulation
    /// has been dropped.
    pub fn buffer_send(&self, connection_id: ConnectionId, data: Vec<u8>) -> SimulationResult<()> {
        let sim = self.upgrade()?;
        sim.buffer_send(connection_id, data)
    }

    /// Get a network provider for the simulation.
    ///
    /// Returns `Err(SimulationError::SimulationShutdown)` if the simulation
    /// has been dropped.
    pub fn network_provider(&self) -> SimulationResult<SimNetworkProvider> {
        let sim = self.upgrade()?;
        Ok(sim.network_provider())
    }

    /// Get a time provider for the simulation.
    ///
    /// Returns `Err(SimulationError::SimulationShutdown)` if the simulation
    /// has been dropped.
    pub fn time_provider(&self) -> SimulationResult<crate::time::SimTimeProvider> {
        let sim = self.upgrade()?;
        Ok(sim.time_provider())
    }

    /// Sleep for the specified duration in simulation time.
    ///
    /// Returns `Err(SimulationError::SimulationShutdown)` if the simulation
    /// has been dropped.
    pub fn sleep(&self, duration: Duration) -> SimulationResult<SleepFuture> {
        let sim = self.upgrade()?;
        Ok(sim.sleep(duration))
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
