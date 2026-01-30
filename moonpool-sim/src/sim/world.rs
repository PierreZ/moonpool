//! Core simulation world and coordination logic.
//!
//! This module provides the central SimWorld coordinator that manages time,
//! event processing, and network simulation state.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    net::IpAddr,
    rc::{Rc, Weak},
    task::Waker,
    time::Duration,
};
use tracing::instrument;

use crate::{
    SimulationError, SimulationResult,
    network::{
        NetworkConfiguration, PartitionStrategy,
        sim::{ConnectionId, ListenerId, SimNetworkProvider},
    },
};

use super::{
    events::{
        ConnectionStateChange, Event, EventQueue, NetworkOperation, ScheduledEvent,
        StorageOperation,
    },
    rng::{reset_sim_rng, set_sim_seed, sim_random, sim_random_range},
    sleep::SleepFuture,
    state::{
        ClogState, CloseReason, ConnectionState, FileId, ListenerState, NetworkState,
        PartitionState, PendingOpType, PendingStorageOp, StorageState,
    },
    wakers::WakerRegistry,
};

/// Internal simulation state holder
#[derive(Debug)]
pub(crate) struct SimInner {
    pub(crate) current_time: Duration,
    /// Drifted timer time (can be ahead of current_time)
    /// FDB ref: sim2.actor.cpp:1058-1064 - timer() drifts 0-0.1s ahead of now()
    pub(crate) timer_time: Duration,
    pub(crate) event_queue: EventQueue,
    pub(crate) next_sequence: u64,

    // Network management
    pub(crate) network: NetworkState,

    // Storage management
    pub(crate) storage: StorageState,

    // Async coordination
    pub(crate) wakers: WakerRegistry,

    // Task management for sleep functionality
    pub(crate) next_task_id: u64,
    pub(crate) awakened_tasks: HashSet<u64>,

    // Event processing metrics
    pub(crate) events_processed: u64,

    // Chaos tracking
    pub(crate) last_bit_flip_time: Duration,
}

impl SimInner {
    pub(crate) fn new() -> Self {
        Self {
            current_time: Duration::ZERO,
            timer_time: Duration::ZERO,
            event_queue: EventQueue::new(),
            next_sequence: 0,
            network: NetworkState::new(NetworkConfiguration::default()),
            storage: StorageState::default(),
            wakers: WakerRegistry::default(),
            next_task_id: 0,
            awakened_tasks: HashSet::new(),
            events_processed: 0,
            last_bit_flip_time: Duration::ZERO,
        }
    }

    pub(crate) fn new_with_config(network_config: NetworkConfiguration) -> Self {
        Self {
            current_time: Duration::ZERO,
            timer_time: Duration::ZERO,
            event_queue: EventQueue::new(),
            next_sequence: 0,
            network: NetworkState::new(network_config),
            storage: StorageState::default(),
            wakers: WakerRegistry::default(),
            next_task_id: 0,
            awakened_tasks: HashSet::new(),
            events_processed: 0,
            last_bit_flip_time: Duration::ZERO,
        }
    }

    /// Calculate the number of bits to flip using a power-law distribution.
    ///
    /// Uses the formula: 32 - floor(log2(random_value))
    /// This creates a power-law distribution biased toward fewer bits:
    /// - 1-2 bits: very common
    /// - 16 bits: uncommon
    /// - 32 bits: very rare
    ///
    /// Matches FDB's approach in FlowTransport.actor.cpp:1297
    pub(crate) fn calculate_flip_bit_count(random_value: u32, min_bits: u32, max_bits: u32) -> u32 {
        if random_value == 0 {
            // Handle edge case: treat 0 as if it were 1
            return max_bits.min(32);
        }

        // Formula: 32 - floor(log2(x)) = 1 + leading_zeros(x)
        let bit_count = 1 + random_value.leading_zeros();

        // Clamp to configured range
        bit_count.clamp(min_bits, max_bits)
    }
}

/// The central simulation coordinator that manages time and event processing.
///
/// `SimWorld` owns all mutable simulation state and provides the main interface
/// for scheduling events and advancing simulation time. It uses a centralized
/// ownership model with handle-based access to avoid borrow checker conflicts.
#[derive(Debug)]
pub struct SimWorld {
    pub(crate) inner: Rc<RefCell<SimInner>>,
}

impl SimWorld {
    /// Internal constructor that handles all initialization logic.
    fn create(network_config: Option<NetworkConfiguration>, seed: u64) -> Self {
        reset_sim_rng();
        set_sim_seed(seed);
        crate::chaos::assertions::reset_assertion_results();

        let inner = match network_config {
            Some(config) => SimInner::new_with_config(config),
            None => SimInner::new(),
        };

        Self {
            inner: Rc::new(RefCell::new(inner)),
        }
    }

    /// Creates a new simulation world with default network configuration.
    ///
    /// Uses default seed (0) for reproducible testing. For custom seeds,
    /// use [`SimWorld::new_with_seed`].
    pub fn new() -> Self {
        Self::create(None, 0)
    }

    /// Creates a new simulation world with a specific seed for deterministic randomness.
    ///
    /// This method ensures clean thread-local RNG state by resetting before
    /// setting the seed, making it safe for consecutive simulations on the same thread.
    ///
    /// # Parameters
    ///
    /// * `seed` - The seed value for deterministic randomness
    pub fn new_with_seed(seed: u64) -> Self {
        Self::create(None, seed)
    }

    /// Creates a new simulation world with custom network configuration.
    pub fn new_with_network_config(network_config: NetworkConfiguration) -> Self {
        Self::create(Some(network_config), 0)
    }

    /// Creates a new simulation world with both custom network configuration and seed.
    ///
    /// # Parameters
    ///
    /// * `network_config` - Network configuration for latency and fault simulation
    /// * `seed` - The seed value for deterministic randomness
    pub fn new_with_network_config_and_seed(
        network_config: NetworkConfiguration,
        seed: u64,
    ) -> Self {
        Self::create(Some(network_config), seed)
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

            // Trigger random partitions based on configuration
            Self::randomly_trigger_partitions_with_inner(&mut inner);

            // Process the event with the mutable reference
            Self::process_event_with_inner(&mut inner, scheduled_event.into_event());

            // Return true if more events are available
            !inner.event_queue.is_empty()
        } else {
            // No more events to process
            false
        }
    }

    /// Processes all scheduled events until the queue is empty or only infrastructure events remain.
    ///
    /// This method processes all workload-related events but stops early if only infrastructure
    /// events (like connection restoration) remain. This prevents infinite loops where
    /// infrastructure events keep the simulation running indefinitely after workloads complete.
    #[instrument(skip(self))]
    pub fn run_until_empty(&mut self) {
        while self.step() {
            // Periodically check if we should stop early (every 50 events for performance)
            if self.inner.borrow().events_processed.is_multiple_of(50) {
                let has_workload_events = !self
                    .inner
                    .borrow()
                    .event_queue
                    .has_only_infrastructure_events();
                if !has_workload_events {
                    tracing::debug!(
                        "Early termination: only infrastructure events remain in queue"
                    );
                    break;
                }
            }
        }
    }

    /// Returns the current simulation time.
    pub fn current_time(&self) -> Duration {
        self.inner.borrow().current_time
    }

    /// Returns the exact simulation time (equivalent to FDB's now()).
    ///
    /// This is the canonical simulation time used for scheduling events.
    /// Use this for precise time comparisons and scheduling.
    pub fn now(&self) -> Duration {
        self.inner.borrow().current_time
    }

    /// Returns the drifted timer time (equivalent to FDB's timer()).
    ///
    /// The timer can be up to `clock_drift_max` (default 100ms) ahead of `now()`.
    /// This simulates real-world clock drift between processes, which is important
    /// for testing time-sensitive code like:
    /// - Timeout handling
    /// - Lease expiration
    /// - Distributed consensus (leader election)
    /// - Cache invalidation
    /// - Heartbeat detection
    ///
    /// FDB formula: `timerTime += random01() * (time + 0.1 - timerTime) / 2.0`
    ///
    /// FDB ref: sim2.actor.cpp:1058-1064
    pub fn timer(&self) -> Duration {
        let mut inner = self.inner.borrow_mut();
        let chaos = &inner.network.config.chaos;

        // If clock drift is disabled, return exact simulation time
        if !chaos.clock_drift_enabled {
            return inner.current_time;
        }

        // FDB formula: timerTime += random01() * (time + 0.1 - timerTime) / 2.0
        // This smoothly interpolates timerTime toward (time + drift_max)
        // The /2.0 creates a damped approach (never overshoots)
        let max_timer = inner.current_time + chaos.clock_drift_max;

        // Only advance if timer is behind max
        if inner.timer_time < max_timer {
            let random_factor = sim_random::<f64>(); // 0.0 to 1.0
            let gap = (max_timer - inner.timer_time).as_secs_f64();
            let delta = random_factor * gap / 2.0;
            inner.timer_time += Duration::from_secs_f64(delta);
        }

        // Ensure timer never goes backwards relative to simulation time
        inner.timer_time = inner.timer_time.max(inner.current_time);

        inner.timer_time
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
    pub fn time_provider(&self) -> crate::providers::SimTimeProvider {
        crate::providers::SimTimeProvider::new(self.downgrade())
    }

    /// Create a task provider for this simulation
    pub fn task_provider(&self) -> crate::TokioTaskProvider {
        crate::TokioTaskProvider
    }

    /// Access network configuration for latency calculations using thread-local RNG.
    ///
    /// This method provides access to the network configuration for calculating
    /// latencies and other network parameters. Random values should be generated
    /// using the thread-local RNG functions like `sim_random()`.
    ///
    /// Access the network configuration for this simulation.
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
    /// write operations on a single connection are processed in FIFO order.
    pub(crate) fn buffer_send(
        &self,
        connection_id: ConnectionId,
        data: Vec<u8>,
    ) -> SimulationResult<()> {
        tracing::debug!(
            "buffer_send called for connection_id={} with {} bytes",
            connection_id.0,
            data.len()
        );
        let mut inner = self.inner.borrow_mut();

        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            // Always add data to send buffer for TCP ordering
            conn.send_buffer.push_back(data);
            tracing::debug!(
                "buffer_send: added data to send_buffer, new length: {}",
                conn.send_buffer.len()
            );

            // If sender is not already active, start processing the buffer
            if !conn.send_in_progress {
                tracing::debug!(
                    "buffer_send: sender not in progress, scheduling ProcessSendBuffer event"
                );
                conn.send_in_progress = true;

                // Schedule immediate processing of the buffer
                let scheduled_time = inner.current_time + std::time::Duration::ZERO;
                let sequence = inner.next_sequence;
                inner.next_sequence += 1;
                let scheduled_event = ScheduledEvent::new(
                    scheduled_time,
                    Event::Network {
                        connection_id: connection_id.0,
                        operation: NetworkOperation::ProcessSendBuffer,
                    },
                    sequence,
                );
                inner.event_queue.schedule(scheduled_event);
                tracing::debug!(
                    "buffer_send: scheduled ProcessSendBuffer event with sequence {}",
                    sequence
                );
            } else {
                tracing::debug!(
                    "buffer_send: sender already in progress, not scheduling new event"
                );
            }

            Ok(())
        } else {
            tracing::debug!(
                "buffer_send: connection_id={} not found in connections table",
                connection_id.0
            );
            Err(SimulationError::InvalidState(
                "connection not found".to_string(),
            ))
        }
    }

    /// Create a bidirectional TCP connection pair for client-server communication.
    ///
    /// FDB Pattern (sim2.actor.cpp:1149-1175):
    /// - Client connection stores server's real address as peer_address
    /// - Server connection stores synthesized ephemeral address (random IP + port 40000-60000)
    ///
    /// This simulates real TCP behavior where servers see client ephemeral ports.
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

        // Parse IP addresses for partition tracking
        let client_ip = NetworkState::parse_ip_from_addr(&client_addr);
        let server_ip = NetworkState::parse_ip_from_addr(&server_addr);

        // FDB Pattern: Synthesize ephemeral address for server-side connection
        // sim2.actor.cpp:1149-1175: randomInt(0,256) for IP offset, randomInt(40000,60000) for port
        // Use thread-local sim_random_range for deterministic randomness
        let ephemeral_peer_addr = match client_ip {
            Some(std::net::IpAddr::V4(ipv4)) => {
                let octets = ipv4.octets();
                let ip_offset = sim_random_range(0u32..256) as u8;
                let new_last_octet = octets[3].wrapping_add(ip_offset);
                let ephemeral_ip =
                    std::net::Ipv4Addr::new(octets[0], octets[1], octets[2], new_last_octet);
                let ephemeral_port = sim_random_range(40000u16..60000);
                format!("{}:{}", ephemeral_ip, ephemeral_port)
            }
            Some(std::net::IpAddr::V6(ipv6)) => {
                // For IPv6, just modify the last segment
                let segments = ipv6.segments();
                let mut new_segments = segments;
                let ip_offset = sim_random_range(0u16..256);
                new_segments[7] = new_segments[7].wrapping_add(ip_offset);
                let ephemeral_ip = std::net::Ipv6Addr::from(new_segments);
                let ephemeral_port = sim_random_range(40000u16..60000);
                format!("[{}]:{}", ephemeral_ip, ephemeral_port)
            }
            None => {
                // Fallback: use client address with random port
                let ephemeral_port = sim_random_range(40000u16..60000);
                format!("unknown:{}", ephemeral_port)
            }
        };

        // Calculate send buffer capacity based on BDP (Bandwidth-Delay Product)
        // Using a default of 64KB which is typical for TCP socket buffers
        // In real simulations this could be: max_latency_ms * bandwidth_bytes_per_ms
        const DEFAULT_SEND_BUFFER_CAPACITY: usize = 64 * 1024; // 64KB

        // Create paired connections
        // Client stores server's real address as peer_address
        inner.network.connections.insert(
            client_id,
            ConnectionState {
                id: client_id,
                addr: client_addr,
                local_ip: client_ip,
                remote_ip: server_ip,
                peer_address: server_addr.clone(),
                receive_buffer: VecDeque::new(),
                paired_connection: Some(server_id),
                send_buffer: VecDeque::new(),
                send_in_progress: false,
                next_send_time: current_time,
                is_closed: false,
                send_closed: false,
                recv_closed: false,
                is_cut: false,
                cut_expiry: None,
                close_reason: CloseReason::None,
                send_buffer_capacity: DEFAULT_SEND_BUFFER_CAPACITY,
                send_delay: None,
                recv_delay: None,
                is_half_open: false,
                half_open_error_at: None,
                is_stable: false,
            },
        );

        // Server stores synthesized ephemeral address as peer_address
        inner.network.connections.insert(
            server_id,
            ConnectionState {
                id: server_id,
                addr: server_addr,
                local_ip: server_ip,
                remote_ip: client_ip,
                peer_address: ephemeral_peer_addr,
                receive_buffer: VecDeque::new(),
                paired_connection: Some(client_id),
                send_buffer: VecDeque::new(),
                send_in_progress: false,
                next_send_time: current_time,
                is_closed: false,
                send_closed: false,
                recv_closed: false,
                is_cut: false,
                cut_expiry: None,
                close_reason: CloseReason::None,
                send_buffer_capacity: DEFAULT_SEND_BUFFER_CAPACITY,
                send_delay: None,
                recv_delay: None,
                is_half_open: false,
                half_open_error_at: None,
                is_stable: false,
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
        let is_replacement = inner.wakers.read_wakers.contains_key(&connection_id);
        inner.wakers.read_wakers.insert(connection_id, waker);
        tracing::debug!(
            "register_read_waker: connection_id={}, replacement={}, total_wakers={}",
            connection_id.0,
            is_replacement,
            inner.wakers.read_wakers.len()
        );
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

    /// Get the peer address for a connection.
    ///
    /// FDB Pattern (sim2.actor.cpp):
    /// - For client-side connections: returns server's listening address
    /// - For server-side connections: returns synthesized ephemeral address
    ///
    /// The returned address may not be connectable for server-side connections,
    /// matching real TCP behavior where servers see client ephemeral ports.
    pub(crate) fn get_connection_peer_address(
        &self,
        connection_id: ConnectionId,
    ) -> Option<String> {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .map(|conn| conn.peer_address.clone())
    }

    /// Sleep for the specified duration in simulation time.
    ///
    /// Returns a future that will complete when the simulation time has advanced
    /// by the specified duration.
    #[instrument(skip(self))]
    pub fn sleep(&self, duration: Duration) -> SleepFuture {
        let task_id = self.generate_task_id();

        // Apply buggified delay if enabled
        let actual_duration = self.apply_buggified_delay(duration);

        // Schedule a wake event for this task
        self.schedule_event(Event::Timer { task_id }, actual_duration);

        // Return a future that will be woken when the event is processed
        SleepFuture::new(self.downgrade(), task_id)
    }

    /// Apply buggified delay to a duration if chaos is enabled.
    fn apply_buggified_delay(&self, duration: Duration) -> Duration {
        let inner = self.inner.borrow();
        let chaos = &inner.network.config.chaos;

        if !chaos.buggified_delay_enabled || chaos.buggified_delay_max == Duration::ZERO {
            return duration;
        }

        // 25% probability per FDB
        if sim_random::<f64>() < chaos.buggified_delay_probability {
            // Power-law distribution: pow(random01(), 1000) creates very skewed delays
            let random_factor = sim_random::<f64>().powf(1000.0);
            let extra_delay = chaos.buggified_delay_max.mul_f64(random_factor);
            tracing::trace!(
                extra_delay_ms = extra_delay.as_millis(),
                "Buggified delay applied"
            );
            duration + extra_delay
        } else {
            duration
        }
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
    pub(crate) fn is_task_awake(&self, task_id: u64) -> SimulationResult<bool> {
        let inner = self.inner.borrow();
        Ok(inner.awakened_tasks.contains(&task_id))
    }

    /// Register a waker for a task.
    pub(crate) fn register_task_waker(&self, task_id: u64, waker: Waker) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.wakers.task_wakers.insert(task_id, waker);
        Ok(())
    }

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

    /// Static event processor for simulation events.
    #[instrument(skip(inner))]
    fn process_event_with_inner(inner: &mut SimInner, event: Event) {
        inner.events_processed += 1;

        match event {
            Event::Timer { task_id } => Self::handle_timer_event(inner, task_id),
            Event::Connection { id, state } => Self::handle_connection_event(inner, id, state),
            Event::Network {
                connection_id,
                operation,
            } => Self::handle_network_event(inner, connection_id, operation),
            Event::Storage { file_id, operation } => {
                Self::handle_storage_event(inner, file_id, operation)
            }
            Event::Shutdown => Self::handle_shutdown_event(inner),
        }
    }

    /// Handle timer events - wake sleeping tasks.
    fn handle_timer_event(inner: &mut SimInner, task_id: u64) {
        inner.awakened_tasks.insert(task_id);
        if let Some(waker) = inner.wakers.task_wakers.remove(&task_id) {
            waker.wake();
        }
    }

    /// Handle connection state change events.
    fn handle_connection_event(inner: &mut SimInner, id: u64, state: ConnectionStateChange) {
        let connection_id = ConnectionId(id);

        match state {
            ConnectionStateChange::BindComplete | ConnectionStateChange::ConnectionReady => {
                // No action needed for these states
            }
            ConnectionStateChange::ClogClear => {
                inner.network.connection_clogs.remove(&connection_id);
                Self::wake_all(&mut inner.wakers.clog_wakers, connection_id);
            }
            ConnectionStateChange::ReadClogClear => {
                inner.network.read_clogs.remove(&connection_id);
                Self::wake_all(&mut inner.wakers.read_clog_wakers, connection_id);
            }
            ConnectionStateChange::PartitionRestore => {
                Self::clear_expired_partitions(inner);
            }
            ConnectionStateChange::SendPartitionClear => {
                Self::clear_expired_send_partitions(inner);
            }
            ConnectionStateChange::RecvPartitionClear => {
                Self::clear_expired_recv_partitions(inner);
            }
            ConnectionStateChange::CutRestore => {
                if let Some(conn) = inner.network.connections.get_mut(&connection_id)
                    && conn.is_cut
                {
                    conn.is_cut = false;
                    conn.cut_expiry = None;
                    tracing::debug!("Connection {} restored via scheduled event", id);
                    Self::wake_all(&mut inner.wakers.cut_wakers, connection_id);
                }
            }
            ConnectionStateChange::HalfOpenError => {
                tracing::debug!("Connection {} half-open error time reached", id);
                if let Some(waker) = inner.wakers.read_wakers.remove(&connection_id) {
                    waker.wake();
                }
                Self::wake_all(&mut inner.wakers.send_buffer_wakers, connection_id);
            }
        }
    }

    /// Clear expired IP partitions.
    fn clear_expired_partitions(inner: &mut SimInner) {
        let now = inner.current_time;
        let expired: Vec<_> = inner
            .network
            .ip_partitions
            .iter()
            .filter_map(|(pair, state)| (now >= state.expires_at).then_some(*pair))
            .collect();

        for pair in expired {
            inner.network.ip_partitions.remove(&pair);
            tracing::debug!("Restored IP partition {} -> {}", pair.0, pair.1);
        }
    }

    /// Clear expired send partitions.
    fn clear_expired_send_partitions(inner: &mut SimInner) {
        let now = inner.current_time;
        let expired: Vec<_> = inner
            .network
            .send_partitions
            .iter()
            .filter_map(|(ip, &expires_at)| (now >= expires_at).then_some(*ip))
            .collect();

        for ip in expired {
            inner.network.send_partitions.remove(&ip);
            tracing::debug!("Cleared send partition for {}", ip);
        }
    }

    /// Clear expired receive partitions.
    fn clear_expired_recv_partitions(inner: &mut SimInner) {
        let now = inner.current_time;
        let expired: Vec<_> = inner
            .network
            .recv_partitions
            .iter()
            .filter_map(|(ip, &expires_at)| (now >= expires_at).then_some(*ip))
            .collect();

        for ip in expired {
            inner.network.recv_partitions.remove(&ip);
            tracing::debug!("Cleared receive partition for {}", ip);
        }
    }

    /// Handle network events (data delivery and send buffer processing).
    fn handle_network_event(inner: &mut SimInner, conn_id: u64, operation: NetworkOperation) {
        let connection_id = ConnectionId(conn_id);

        match operation {
            NetworkOperation::DataDelivery { data } => {
                Self::handle_data_delivery(inner, connection_id, data);
            }
            NetworkOperation::ProcessSendBuffer => {
                Self::handle_process_send_buffer(inner, connection_id);
            }
        }
    }

    /// Handle data delivery to a connection's receive buffer.
    fn handle_data_delivery(inner: &mut SimInner, connection_id: ConnectionId, data: Vec<u8>) {
        tracing::trace!(
            "DataDelivery: {} bytes to connection {}",
            data.len(),
            connection_id.0
        );

        // Check connection exists and if it's stable
        let is_stable = inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.is_stable);

        if !inner.network.connections.contains_key(&connection_id) {
            tracing::warn!("DataDelivery: connection {} not found", connection_id.0);
            return;
        }

        // Apply bit flipping chaos (needs mutable inner access) - skip for stable connections
        let data_to_deliver = if is_stable {
            data
        } else {
            Self::maybe_corrupt_data(inner, &data)
        };

        // Now get connection reference and deliver data
        let Some(conn) = inner.network.connections.get_mut(&connection_id) else {
            return;
        };

        for &byte in &data_to_deliver {
            conn.receive_buffer.push_back(byte);
        }

        if let Some(waker) = inner.wakers.read_wakers.remove(&connection_id) {
            waker.wake();
        }
    }

    /// Maybe corrupt data with bit flips based on chaos configuration.
    fn maybe_corrupt_data(inner: &mut SimInner, data: &[u8]) -> Vec<u8> {
        if data.is_empty() {
            return data.to_vec();
        }

        let chaos = &inner.network.config.chaos;
        let now = inner.current_time;
        let cooldown_elapsed =
            now.saturating_sub(inner.last_bit_flip_time) >= chaos.bit_flip_cooldown;

        if !cooldown_elapsed || !crate::buggify_with_prob!(chaos.bit_flip_probability) {
            return data.to_vec();
        }

        let random_value = sim_random::<u32>();
        let flip_count = SimInner::calculate_flip_bit_count(
            random_value,
            chaos.bit_flip_min_bits,
            chaos.bit_flip_max_bits,
        );

        let mut corrupted_data = data.to_vec();
        let mut flipped_positions = std::collections::HashSet::new();

        for _ in 0..flip_count {
            let byte_idx = (sim_random::<u64>() as usize) % corrupted_data.len();
            let bit_idx = (sim_random::<u64>() as usize) % 8;
            let position = (byte_idx, bit_idx);

            if !flipped_positions.contains(&position) {
                flipped_positions.insert(position);
                corrupted_data[byte_idx] ^= 1 << bit_idx;
            }
        }

        inner.last_bit_flip_time = now;
        tracing::info!(
            "BitFlipInjected: bytes={} bits_flipped={} unique_positions={}",
            data.len(),
            flip_count,
            flipped_positions.len()
        );

        corrupted_data
    }

    /// Handle processing of a connection's send buffer.
    fn handle_process_send_buffer(inner: &mut SimInner, connection_id: ConnectionId) {
        let is_partitioned = inner
            .network
            .is_connection_partitioned(connection_id, inner.current_time);

        if is_partitioned {
            Self::handle_partitioned_send(inner, connection_id);
        } else {
            Self::handle_normal_send(inner, connection_id);
        }
    }

    /// Handle send when connection is partitioned.
    fn handle_partitioned_send(inner: &mut SimInner, connection_id: ConnectionId) {
        let Some(conn) = inner.network.connections.get_mut(&connection_id) else {
            return;
        };

        if let Some(data) = conn.send_buffer.pop_front() {
            tracing::debug!(
                "Connection {} partitioned, failing send of {} bytes",
                connection_id.0,
                data.len()
            );
            Self::wake_all(&mut inner.wakers.send_buffer_wakers, connection_id);

            if !conn.send_buffer.is_empty() {
                Self::schedule_process_send_buffer(inner, connection_id);
            } else {
                conn.send_in_progress = false;
            }
        } else {
            conn.send_in_progress = false;
        }
    }

    /// Handle normal (non-partitioned) send.
    fn handle_normal_send(inner: &mut SimInner, connection_id: ConnectionId) {
        // Extract connection info first
        let Some(conn) = inner.network.connections.get(&connection_id) else {
            return;
        };

        let paired_id = conn.paired_connection;
        let send_delay = conn.send_delay;
        let next_send_time = conn.next_send_time;
        let has_data = !conn.send_buffer.is_empty();
        let is_stable = conn.is_stable; // For stable connection checks

        let recv_delay = paired_id.and_then(|pid| {
            inner
                .network
                .connections
                .get(&pid)
                .and_then(|c| c.recv_delay)
        });

        if !has_data {
            if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
                conn.send_in_progress = false;
            }
            return;
        }

        let Some(conn) = inner.network.connections.get_mut(&connection_id) else {
            return;
        };

        let Some(mut data) = conn.send_buffer.pop_front() else {
            conn.send_in_progress = false;
            return;
        };

        Self::wake_all(&mut inner.wakers.send_buffer_wakers, connection_id);

        // BUGGIFY: Simulate partial/short writes (skip for stable connections)
        if !is_stable && crate::buggify!() && !data.is_empty() {
            let max_send = std::cmp::min(
                data.len(),
                inner.network.config.chaos.partial_write_max_bytes,
            );
            let truncate_to = sim_random_range(0..max_send + 1);

            if truncate_to < data.len() {
                let remainder = data.split_off(truncate_to);
                conn.send_buffer.push_front(remainder);
                tracing::debug!(
                    "BUGGIFY: Partial write on connection {} - sending {} bytes",
                    connection_id.0,
                    data.len()
                );
            }
        }

        let has_more = !conn.send_buffer.is_empty();
        let base_delay = if has_more {
            Duration::from_nanos(1)
        } else {
            send_delay.unwrap_or_else(|| {
                crate::network::sample_duration(&inner.network.config.write_latency)
            })
        };

        let earliest_time = std::cmp::max(inner.current_time + base_delay, next_send_time);
        conn.next_send_time = earliest_time + Duration::from_nanos(1);

        // Schedule data delivery to paired connection
        if let Some(paired_id) = paired_id {
            let scheduled_time = earliest_time + recv_delay.unwrap_or(Duration::ZERO);
            let sequence = inner.next_sequence;
            inner.next_sequence += 1;

            inner.event_queue.schedule(ScheduledEvent::new(
                scheduled_time,
                Event::Network {
                    connection_id: paired_id.0,
                    operation: NetworkOperation::DataDelivery { data },
                },
                sequence,
            ));
        }

        // Schedule next send if more data
        if !conn.send_buffer.is_empty() {
            Self::schedule_process_send_buffer(inner, connection_id);
        } else {
            conn.send_in_progress = false;
        }
    }

    /// Schedule a ProcessSendBuffer event for the given connection.
    fn schedule_process_send_buffer(inner: &mut SimInner, connection_id: ConnectionId) {
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        inner.event_queue.schedule(ScheduledEvent::new(
            inner.current_time,
            Event::Network {
                connection_id: connection_id.0,
                operation: NetworkOperation::ProcessSendBuffer,
            },
            sequence,
        ));
    }

    /// Handle storage I/O events.
    ///
    /// Storage events represent the completion of I/O operations.
    /// Processing applies faults and wakes waiting tasks.
    fn handle_storage_event(inner: &mut SimInner, file_id: u64, operation: StorageOperation) {
        let file_id = FileId(file_id);

        match operation {
            StorageOperation::ReadComplete { len: _ } => {
                Self::handle_read_complete(inner, file_id);
            }
            StorageOperation::WriteComplete { len: _ } => {
                Self::handle_write_complete(inner, file_id);
            }
            StorageOperation::SyncComplete => {
                Self::handle_sync_complete(inner, file_id);
            }
            StorageOperation::OpenComplete => {
                Self::handle_open_complete(inner, file_id);
            }
            StorageOperation::SetLenComplete { new_len } => {
                Self::handle_set_len_complete(inner, file_id, new_len);
            }
        }
    }

    /// Handle read operation completion.
    fn handle_read_complete(inner: &mut SimInner, file_id: FileId) {
        // Find and remove the oldest pending read operation
        let op_seq = if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
            // Find the first pending read operation
            let op_seq = file_state
                .pending_ops
                .iter()
                .find(|(_, op)| op.op_type == PendingOpType::Read)
                .map(|(&seq, _)| seq);

            if let Some(seq) = op_seq {
                file_state.pending_ops.remove(&seq);
            }
            op_seq
        } else {
            tracing::warn!("ReadComplete for unknown file {:?}", file_id);
            return;
        };

        // Wake the waker for this operation
        if let Some(seq) = op_seq
            && let Some(waker) = inner.wakers.storage_wakers.remove(&(file_id, seq))
        {
            tracing::trace!("Waking read waker for file {:?}, op {}", file_id, seq);
            waker.wake();
        }
    }

    /// Handle write operation completion.
    ///
    /// Applies the write to storage with potential fault injection:
    /// - phantom_write_probability: write appears to succeed but isn't persisted
    /// - misdirect_write_probability: write lands at wrong location
    fn handle_write_complete(inner: &mut SimInner, file_id: FileId) {
        let config = inner.storage.config.clone();

        // Find the oldest pending write operation and extract its data
        let op_info = if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
            let op_seq = file_state
                .pending_ops
                .iter()
                .find(|(_, op)| op.op_type == PendingOpType::Write)
                .map(|(&seq, _)| seq);

            if let Some(seq) = op_seq {
                let op = file_state.pending_ops.remove(&seq);
                op.map(|o| (seq, o.offset, o.data))
            } else {
                None
            }
        } else {
            tracing::warn!("WriteComplete for unknown file {:?}", file_id);
            return;
        };

        // Apply the write with potential fault injection
        if let Some((op_seq, offset, data_opt)) = op_info {
            if let Some(data) = data_opt
                && let Some(file_state) = inner.storage.files.get_mut(&file_id)
            {
                // Check for phantom write (write appears to succeed but doesn't persist)
                if sim_random::<f64>() < config.phantom_write_probability {
                    tracing::info!(
                        "Phantom write injected for file {:?}, offset {}, len {}",
                        file_id,
                        offset,
                        data.len()
                    );
                    file_state.storage.record_phantom_write(offset, &data);
                }
                // Check for misdirected write
                else if sim_random::<f64>() < config.misdirect_write_probability {
                    // Pick a random different offset
                    let max_offset = file_state.storage.size().saturating_sub(data.len() as u64);
                    let mistaken_offset = if max_offset > 0 {
                        sim_random_range(0..max_offset)
                    } else {
                        0
                    };
                    tracing::info!(
                        "Misdirected write injected for file {:?}: intended={}, actual={}",
                        file_id,
                        offset,
                        mistaken_offset
                    );
                    if let Err(e) =
                        file_state
                            .storage
                            .apply_misdirected_write(offset, mistaken_offset, &data)
                    {
                        tracing::warn!("Failed to apply misdirected write: {}", e);
                    }
                }
                // Normal write (not synced - may be lost on crash)
                else if let Err(e) = file_state.storage.write(offset, &data, false) {
                    tracing::warn!("Write failed for file {:?}: {}", file_id, e);
                }
            }

            // Wake the waker for this operation
            if let Some(waker) = inner.wakers.storage_wakers.remove(&(file_id, op_seq)) {
                tracing::trace!("Waking write waker for file {:?}, op {}", file_id, op_seq);
                waker.wake();
            }
        }
    }

    /// Handle sync operation completion.
    ///
    /// Applies sync_failure_probability fault injection.
    fn handle_sync_complete(inner: &mut SimInner, file_id: FileId) {
        let sync_failure_prob = inner.storage.config.sync_failure_probability;

        // Find and remove the oldest pending sync operation
        let op_seq = if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
            let op_seq = file_state
                .pending_ops
                .iter()
                .find(|(_, op)| op.op_type == PendingOpType::Sync)
                .map(|(&seq, _)| seq);

            if let Some(seq) = op_seq {
                file_state.pending_ops.remove(&seq);

                // Check for sync failure
                if sim_random::<f64>() < sync_failure_prob {
                    tracing::info!("Sync failure injected for file {:?}", file_id);
                    // On sync failure, we don't call storage.sync()
                    // Data remains in pending state and may be lost on crash
                } else {
                    // Successful sync - make all pending writes durable
                    file_state.storage.sync();
                }
            }
            op_seq
        } else {
            tracing::warn!("SyncComplete for unknown file {:?}", file_id);
            return;
        };

        // Wake the waker for this operation
        if let Some(seq) = op_seq
            && let Some(waker) = inner.wakers.storage_wakers.remove(&(file_id, seq))
        {
            tracing::trace!("Waking sync waker for file {:?}, op {}", file_id, seq);
            waker.wake();
        }
    }

    /// Handle open operation completion.
    fn handle_open_complete(inner: &mut SimInner, file_id: FileId) {
        // Find and remove the oldest pending open operation
        let op_seq = if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
            let op_seq = file_state
                .pending_ops
                .iter()
                .find(|(_, op)| op.op_type == PendingOpType::Open)
                .map(|(&seq, _)| seq);

            if let Some(seq) = op_seq {
                file_state.pending_ops.remove(&seq);
            }
            op_seq
        } else {
            // File might not have pending open op (it was already "open" on creation)
            tracing::trace!("OpenComplete for file {:?} (no pending op)", file_id);
            return;
        };

        // Wake the waker for this operation
        if let Some(seq) = op_seq
            && let Some(waker) = inner.wakers.storage_wakers.remove(&(file_id, seq))
        {
            tracing::trace!("Waking open waker for file {:?}, op {}", file_id, seq);
            waker.wake();
        }
    }

    /// Handle set_len operation completion.
    fn handle_set_len_complete(inner: &mut SimInner, file_id: FileId, new_len: u64) {
        // Find and remove the oldest pending set_len operation
        let op_seq = if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
            let op_seq = file_state
                .pending_ops
                .iter()
                .find(|(_, op)| op.op_type == PendingOpType::SetLen)
                .map(|(&seq, _)| seq);

            if let Some(seq) = op_seq {
                file_state.pending_ops.remove(&seq);

                // Resize the storage
                // Note: InMemoryStorage doesn't have a resize method, so we create a new one
                // This is a simplification - in a real implementation we'd preserve existing data
                let seed = sim_random::<u64>();
                let mut new_storage = crate::storage::InMemoryStorage::new(new_len, seed);

                // Copy existing data up to the minimum of old and new sizes
                let copy_len = file_state.storage.size().min(new_len) as usize;
                if copy_len > 0 {
                    let mut buf = vec![0u8; copy_len];
                    if file_state.storage.read(0, &mut buf).is_ok() {
                        let _ = new_storage.write(0, &buf, true);
                    }
                }
                file_state.storage = new_storage;
            }
            op_seq
        } else {
            tracing::warn!("SetLenComplete for unknown file {:?}", file_id);
            return;
        };

        // Wake the waker for this operation
        if let Some(seq) = op_seq
            && let Some(waker) = inner.wakers.storage_wakers.remove(&(file_id, seq))
        {
            tracing::trace!(
                "Waking set_len waker for file {:?}, op {}, new_len={}",
                file_id,
                seq,
                new_len
            );
            waker.wake();
        }
    }

    /// Handle shutdown event - wake all pending tasks.
    fn handle_shutdown_event(inner: &mut SimInner) {
        tracing::debug!("Processing Shutdown event - waking all pending tasks");

        for (task_id, waker) in inner.wakers.task_wakers.drain() {
            tracing::trace!("Waking task {}", task_id);
            waker.wake();
        }

        for (_conn_id, waker) in inner.wakers.read_wakers.drain() {
            waker.wake();
        }

        tracing::debug!("Shutdown event processed");
    }

    /// Get current assertion results for all tracked assertions.
    pub fn assertion_results(
        &self,
    ) -> std::collections::HashMap<String, crate::chaos::AssertionStats> {
        crate::chaos::get_assertion_results()
    }

    /// Reset assertion statistics to empty state.
    pub fn reset_assertion_results(&self) {
        crate::chaos::reset_assertion_results();
    }

    /// Extract simulation metrics for reporting.
    pub fn extract_metrics(&self) -> crate::runner::SimulationMetrics {
        let inner = self.inner.borrow();

        crate::runner::SimulationMetrics {
            wall_time: std::time::Duration::ZERO,
            simulated_time: inner.current_time,
            events_processed: inner.events_processed,
        }
    }

    // Phase 7: Simple write clogging methods

    /// Check if a write should be clogged based on probability
    pub fn should_clog_write(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        let config = &inner.network.config;

        // Skip stable connections (FDB: stableConnection exempt from chaos)
        if inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.is_stable)
        {
            return false;
        }

        // Skip if already clogged
        if let Some(clog_state) = inner.network.connection_clogs.get(&connection_id) {
            return inner.current_time < clog_state.expires_at;
        }

        // Check probability
        config.chaos.clog_probability > 0.0 && sim_random::<f64>() < config.chaos.clog_probability
    }

    /// Clog a connection's write operations
    pub fn clog_write(&self, connection_id: ConnectionId) {
        let mut inner = self.inner.borrow_mut();
        let config = &inner.network.config;

        let clog_duration = crate::network::sample_duration(&config.chaos.clog_duration);
        let expires_at = inner.current_time + clog_duration;
        inner
            .network
            .connection_clogs
            .insert(connection_id, ClogState { expires_at });

        // Schedule an event to clear this clog
        let clear_event = Event::Connection {
            id: connection_id.0,
            state: ConnectionStateChange::ClogClear,
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

    /// Register a waker for when write clog clears
    pub fn register_clog_waker(&self, connection_id: ConnectionId, waker: Waker) {
        let mut inner = self.inner.borrow_mut();
        inner
            .wakers
            .clog_wakers
            .entry(connection_id)
            .or_default()
            .push(waker);
    }

    // Read clogging methods (symmetric with write clogging)

    /// Check if a read should be clogged based on probability
    pub fn should_clog_read(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        let config = &inner.network.config;

        // Skip stable connections (FDB: stableConnection exempt from chaos)
        if inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.is_stable)
        {
            return false;
        }

        // Skip if already clogged
        if let Some(clog_state) = inner.network.read_clogs.get(&connection_id) {
            return inner.current_time < clog_state.expires_at;
        }

        // Check probability (same as write clogging)
        config.chaos.clog_probability > 0.0 && sim_random::<f64>() < config.chaos.clog_probability
    }

    /// Clog a connection's read operations
    pub fn clog_read(&self, connection_id: ConnectionId) {
        let mut inner = self.inner.borrow_mut();
        let config = &inner.network.config;

        let clog_duration = crate::network::sample_duration(&config.chaos.clog_duration);
        let expires_at = inner.current_time + clog_duration;
        inner
            .network
            .read_clogs
            .insert(connection_id, ClogState { expires_at });

        // Schedule an event to clear this read clog
        let clear_event = Event::Connection {
            id: connection_id.0,
            state: ConnectionStateChange::ReadClogClear,
        };
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        inner
            .event_queue
            .schedule(ScheduledEvent::new(expires_at, clear_event, sequence));
    }

    /// Check if a connection's reads are currently clogged
    pub fn is_read_clogged(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();

        if let Some(clog_state) = inner.network.read_clogs.get(&connection_id) {
            inner.current_time < clog_state.expires_at
        } else {
            false
        }
    }

    /// Register a waker for when read clog clears
    pub fn register_read_clog_waker(&self, connection_id: ConnectionId, waker: Waker) {
        let mut inner = self.inner.borrow_mut();
        inner
            .wakers
            .read_clog_wakers
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

    // Connection Cut Methods (temporary network outage simulation)

    /// Temporarily cut a connection for the specified duration.
    ///
    /// Unlike `close_connection`, a cut connection will be automatically restored
    /// after the duration expires. This simulates temporary network outages where
    /// the underlying connection remains but is temporarily unavailable.
    ///
    /// During a cut:
    /// - `poll_read` returns `Poll::Pending` (waits for restore)
    /// - `poll_write` returns `Poll::Pending` (waits for restore)
    /// - Buffered data is preserved
    pub fn cut_connection(&self, connection_id: ConnectionId, duration: Duration) {
        let mut inner = self.inner.borrow_mut();
        let expires_at = inner.current_time + duration;

        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            conn.is_cut = true;
            conn.cut_expiry = Some(expires_at);
            tracing::debug!("Connection {} cut until {:?}", connection_id.0, expires_at);

            // Schedule restoration event
            let restore_event = Event::Connection {
                id: connection_id.0,
                state: ConnectionStateChange::CutRestore,
            };
            let sequence = inner.next_sequence;
            inner.next_sequence += 1;
            inner
                .event_queue
                .schedule(ScheduledEvent::new(expires_at, restore_event, sequence));
        }
    }

    /// Check if a connection is temporarily cut.
    ///
    /// A cut connection is temporarily unavailable but will be restored.
    /// This is different from `is_connection_closed` which indicates permanent closure.
    pub fn is_connection_cut(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| {
                conn.is_cut
                    && conn
                        .cut_expiry
                        .is_some_and(|expiry| inner.current_time < expiry)
            })
    }

    /// Restore a cut connection immediately.
    ///
    /// This cancels the cut state and wakes any tasks waiting for restoration.
    pub fn restore_connection(&self, connection_id: ConnectionId) {
        let mut inner = self.inner.borrow_mut();

        if let Some(conn) = inner.network.connections.get_mut(&connection_id)
            && conn.is_cut
        {
            conn.is_cut = false;
            conn.cut_expiry = None;
            tracing::debug!("Connection {} restored", connection_id.0);

            // Wake any tasks waiting for restoration
            Self::wake_all(&mut inner.wakers.cut_wakers, connection_id);
        }
    }

    /// Register a waker for when a cut connection is restored.
    pub fn register_cut_waker(&self, connection_id: ConnectionId, waker: Waker) {
        let mut inner = self.inner.borrow_mut();
        inner
            .wakers
            .cut_wakers
            .entry(connection_id)
            .or_default()
            .push(waker);
    }

    // Send buffer management methods

    /// Get the send buffer capacity for a connection.
    pub fn send_buffer_capacity(&self, connection_id: ConnectionId) -> usize {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .map(|conn| conn.send_buffer_capacity)
            .unwrap_or(0)
    }

    /// Get the current send buffer usage for a connection.
    pub fn send_buffer_used(&self, connection_id: ConnectionId) -> usize {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .map(|conn| conn.send_buffer.iter().map(|v| v.len()).sum())
            .unwrap_or(0)
    }

    /// Get the available send buffer space for a connection.
    pub fn available_send_buffer(&self, connection_id: ConnectionId) -> usize {
        let capacity = self.send_buffer_capacity(connection_id);
        let used = self.send_buffer_used(connection_id);
        capacity.saturating_sub(used)
    }

    /// Register a waker for when send buffer space becomes available.
    pub fn register_send_buffer_waker(&self, connection_id: ConnectionId, waker: Waker) {
        let mut inner = self.inner.borrow_mut();
        inner
            .wakers
            .send_buffer_wakers
            .entry(connection_id)
            .or_default()
            .push(waker);
    }

    /// Wake any tasks waiting for send buffer space on a connection.
    #[allow(dead_code)] // May be used for external buffer management
    fn wake_send_buffer_waiters(&self, connection_id: ConnectionId) {
        let mut inner = self.inner.borrow_mut();
        Self::wake_all(&mut inner.wakers.send_buffer_wakers, connection_id);
    }

    // Per-IP-pair base latency methods

    /// Get the base latency for a connection pair.
    /// Returns the latency if already set, otherwise None.
    pub fn get_pair_latency(&self, src: IpAddr, dst: IpAddr) -> Option<Duration> {
        let inner = self.inner.borrow();
        inner.network.pair_latencies.get(&(src, dst)).copied()
    }

    /// Set the base latency for a connection pair if not already set.
    /// Returns the latency (existing or newly set).
    pub fn set_pair_latency_if_not_set(
        &self,
        src: IpAddr,
        dst: IpAddr,
        latency: Duration,
    ) -> Duration {
        let mut inner = self.inner.borrow_mut();
        *inner
            .network
            .pair_latencies
            .entry((src, dst))
            .or_insert_with(|| {
                tracing::debug!(
                    "Setting base latency for IP pair {} -> {} to {:?}",
                    src,
                    dst,
                    latency
                );
                latency
            })
    }

    /// Get the base latency for a connection based on its IP pair.
    /// If not set, samples from config and sets it.
    pub fn get_connection_base_latency(&self, connection_id: ConnectionId) -> Duration {
        let inner = self.inner.borrow();
        let (local_ip, remote_ip) = inner
            .network
            .connections
            .get(&connection_id)
            .and_then(|conn| Some((conn.local_ip?, conn.remote_ip?)))
            .unwrap_or({
                (
                    IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                    IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                )
            });
        drop(inner);

        // Check if latency is already set
        if let Some(latency) = self.get_pair_latency(local_ip, remote_ip) {
            return latency;
        }

        // Sample a new latency from config and set it
        let latency = self
            .with_network_config(|config| crate::network::sample_duration(&config.write_latency));
        self.set_pair_latency_if_not_set(local_ip, remote_ip, latency)
    }

    // Per-connection asymmetric delay methods

    /// Get the send delay for a connection.
    /// Returns the per-connection override if set, otherwise None.
    pub fn get_send_delay(&self, connection_id: ConnectionId) -> Option<Duration> {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .and_then(|conn| conn.send_delay)
    }

    /// Get the receive delay for a connection.
    /// Returns the per-connection override if set, otherwise None.
    pub fn get_recv_delay(&self, connection_id: ConnectionId) -> Option<Duration> {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .and_then(|conn| conn.recv_delay)
    }

    /// Set asymmetric delays for a connection.
    /// If a delay is None, the global configuration is used instead.
    pub fn set_asymmetric_delays(
        &self,
        connection_id: ConnectionId,
        send_delay: Option<Duration>,
        recv_delay: Option<Duration>,
    ) {
        let mut inner = self.inner.borrow_mut();
        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            conn.send_delay = send_delay;
            conn.recv_delay = recv_delay;
            tracing::debug!(
                "Connection {} asymmetric delays set: send={:?}, recv={:?}",
                connection_id.0,
                send_delay,
                recv_delay
            );
        }
    }

    /// Check if a connection is permanently closed
    pub fn is_connection_closed(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.is_closed)
    }

    /// Close a connection gracefully (FIN semantics).
    ///
    /// The peer will receive EOF on read operations.
    pub fn close_connection(&self, connection_id: ConnectionId) {
        self.close_connection_with_reason(connection_id, CloseReason::Graceful);
    }

    /// Close a connection abruptly (RST semantics).
    ///
    /// The peer will receive ECONNRESET on both read and write operations.
    pub fn close_connection_abort(&self, connection_id: ConnectionId) {
        self.close_connection_with_reason(connection_id, CloseReason::Aborted);
    }

    /// Get the close reason for a connection.
    pub fn get_close_reason(&self, connection_id: ConnectionId) -> CloseReason {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .map(|conn| conn.close_reason)
            .unwrap_or(CloseReason::None)
    }

    /// Close a connection with a specific close reason.
    fn close_connection_with_reason(&self, connection_id: ConnectionId, reason: CloseReason) {
        let mut inner = self.inner.borrow_mut();

        let paired_connection_id = inner
            .network
            .connections
            .get(&connection_id)
            .and_then(|conn| conn.paired_connection);

        if let Some(conn) = inner.network.connections.get_mut(&connection_id)
            && !conn.is_closed
        {
            conn.is_closed = true;
            conn.close_reason = reason;
            tracing::debug!(
                "Connection {} closed permanently (reason: {:?})",
                connection_id.0,
                reason
            );
        }

        if let Some(paired_id) = paired_connection_id
            && let Some(paired_conn) = inner.network.connections.get_mut(&paired_id)
            && !paired_conn.is_closed
        {
            paired_conn.is_closed = true;
            paired_conn.close_reason = reason;
            tracing::debug!(
                "Paired connection {} also closed (reason: {:?})",
                paired_id.0,
                reason
            );
        }

        if let Some(waker) = inner.wakers.read_wakers.remove(&connection_id) {
            tracing::debug!(
                "Waking read waker for closed connection {}",
                connection_id.0
            );
            waker.wake();
        }

        if let Some(paired_id) = paired_connection_id
            && let Some(paired_waker) = inner.wakers.read_wakers.remove(&paired_id)
        {
            tracing::debug!(
                "Waking read waker for paired closed connection {}",
                paired_id.0
            );
            paired_waker.wake();
        }
    }

    /// Close connection asymmetrically (FDB rollRandomClose pattern)
    pub fn close_connection_asymmetric(
        &self,
        connection_id: ConnectionId,
        close_send: bool,
        close_recv: bool,
    ) {
        let mut inner = self.inner.borrow_mut();

        let paired_id = inner
            .network
            .connections
            .get(&connection_id)
            .and_then(|conn| conn.paired_connection);

        if close_send && let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            conn.send_closed = true;
            conn.send_buffer.clear();
            tracing::debug!(
                "Connection {} send side closed (asymmetric)",
                connection_id.0
            );
        }

        if close_recv
            && let Some(paired) = paired_id
            && let Some(paired_conn) = inner.network.connections.get_mut(&paired)
        {
            paired_conn.recv_closed = true;
            tracing::debug!(
                "Connection {} recv side closed (asymmetric via peer)",
                paired.0
            );
        }

        if close_send && let Some(waker) = inner.wakers.read_wakers.remove(&connection_id) {
            waker.wake();
        }
        if close_recv
            && let Some(paired) = paired_id
            && let Some(waker) = inner.wakers.read_wakers.remove(&paired)
        {
            waker.wake();
        }
    }

    /// Roll random close chaos injection (FDB rollRandomClose pattern)
    pub fn roll_random_close(&self, connection_id: ConnectionId) -> Option<bool> {
        let mut inner = self.inner.borrow_mut();
        let config = &inner.network.config;

        // Skip stable connections (FDB: stableConnection exempt from chaos)
        if inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.is_stable)
        {
            return None;
        }

        if config.chaos.random_close_probability <= 0.0 {
            return None;
        }

        let current_time = inner.current_time;
        let time_since_last = current_time.saturating_sub(inner.network.last_random_close_time);
        if time_since_last < config.chaos.random_close_cooldown {
            return None;
        }

        if !crate::buggify_with_prob!(config.chaos.random_close_probability) {
            return None;
        }

        inner.network.last_random_close_time = current_time;

        let paired_id = inner
            .network
            .connections
            .get(&connection_id)
            .and_then(|conn| conn.paired_connection);

        let a = super::rng::sim_random_f64();
        let close_recv = a < 0.66;
        let close_send = a > 0.33;

        tracing::info!(
            "Random connection failure triggered on connection {} (send={}, recv={}, a={:.3})",
            connection_id.0,
            close_send,
            close_recv,
            a
        );

        if close_send && let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            conn.send_closed = true;
            conn.send_buffer.clear();
        }

        if close_recv
            && let Some(paired) = paired_id
            && let Some(paired_conn) = inner.network.connections.get_mut(&paired)
        {
            paired_conn.recv_closed = true;
        }

        if close_send && let Some(waker) = inner.wakers.read_wakers.remove(&connection_id) {
            waker.wake();
        }
        if close_recv
            && let Some(paired) = paired_id
            && let Some(waker) = inner.wakers.read_wakers.remove(&paired)
        {
            waker.wake();
        }

        let b = super::rng::sim_random_f64();
        let explicit = b < inner.network.config.chaos.random_close_explicit_ratio;

        tracing::debug!(
            "Random close explicit={} (b={:.3}, ratio={:.2})",
            explicit,
            b,
            inner.network.config.chaos.random_close_explicit_ratio
        );

        Some(explicit)
    }

    /// Check if a connection's send side is closed
    pub fn is_send_closed(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.send_closed || conn.is_closed)
    }

    /// Check if a connection's receive side is closed
    pub fn is_recv_closed(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.recv_closed || conn.is_closed)
    }

    // Half-Open Connection Simulation Methods

    /// Simulate a peer crash on a connection.
    ///
    /// This puts the connection in a half-open state where:
    /// - The local side still thinks it's connected
    /// - Writes succeed but data is silently discarded (peer is gone)
    /// - Reads block waiting for data that will never come
    /// - After `error_delay`, both read and write return ECONNRESET
    ///
    /// This simulates real-world scenarios where a remote peer crashes
    /// or becomes unreachable, but the local TCP stack hasn't detected it yet.
    pub fn simulate_peer_crash(&self, connection_id: ConnectionId, error_delay: Duration) {
        let mut inner = self.inner.borrow_mut();
        let current_time = inner.current_time;
        let error_at = current_time + error_delay;

        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            conn.is_half_open = true;
            conn.half_open_error_at = Some(error_at);

            // Clear the paired connection to simulate peer being gone
            // Data sent will go nowhere
            conn.paired_connection = None;

            tracing::info!(
                "Connection {} now half-open, errors manifest at {:?}",
                connection_id.0,
                error_at
            );
        }

        // Schedule an event to wake any waiting readers when error time arrives
        let wake_event = Event::Connection {
            id: connection_id.0,
            state: ConnectionStateChange::HalfOpenError,
        };
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        let scheduled_event = ScheduledEvent::new(error_at, wake_event, sequence);
        inner.event_queue.schedule(scheduled_event);
    }

    /// Check if a connection is in half-open state
    pub fn is_half_open(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.is_half_open)
    }

    /// Check if a half-open connection should return errors now
    pub fn should_half_open_error(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        let current_time = inner.current_time;
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| {
                conn.is_half_open
                    && conn
                        .half_open_error_at
                        .is_some_and(|error_at| current_time >= error_at)
            })
    }

    // Stable Connection Methods

    /// Mark a connection as stable, exempting it from chaos injection.
    ///
    /// Stable connections are exempt from:
    /// - Random close (`roll_random_close`)
    /// - Write clogging
    /// - Read clogging
    /// - Bit flip corruption
    /// - Partial write truncation
    ///
    /// FDB ref: sim2.actor.cpp:357-362 (`stableConnection` flag)
    ///
    /// # Real-World Scenario
    /// Use this for parent-child process connections or supervision channels
    /// that should remain reliable even during chaos testing.
    pub fn mark_connection_stable(&self, connection_id: ConnectionId) {
        let mut inner = self.inner.borrow_mut();
        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            conn.is_stable = true;
            tracing::debug!("Connection {} marked as stable", connection_id.0);

            // Also mark the paired connection as stable
            if let Some(paired_id) = conn.paired_connection
                && let Some(paired_conn) = inner.network.connections.get_mut(&paired_id)
            {
                paired_conn.is_stable = true;
                tracing::debug!("Paired connection {} also marked as stable", paired_id.0);
            }
        }
    }

    /// Check if a connection is marked as stable.
    pub fn is_connection_stable(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.is_stable)
    }

    // Network Partition Control Methods

    /// Partition communication between two IP addresses for a specified duration
    pub fn partition_pair(
        &self,
        from_ip: std::net::IpAddr,
        to_ip: std::net::IpAddr,
        duration: Duration,
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        let expires_at = inner.current_time + duration;

        inner
            .network
            .ip_partitions
            .insert((from_ip, to_ip), PartitionState { expires_at });

        let restore_event = Event::Connection {
            id: 0,
            state: ConnectionStateChange::PartitionRestore,
        };
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        let scheduled_event = ScheduledEvent::new(expires_at, restore_event, sequence);
        inner.event_queue.schedule(scheduled_event);

        tracing::debug!(
            "Partitioned {} -> {} until {:?}",
            from_ip,
            to_ip,
            expires_at
        );
        Ok(())
    }

    /// Block all outgoing communication from an IP address
    pub fn partition_send_from(
        &self,
        ip: std::net::IpAddr,
        duration: Duration,
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        let expires_at = inner.current_time + duration;

        inner.network.send_partitions.insert(ip, expires_at);

        let clear_event = Event::Connection {
            id: 0,
            state: ConnectionStateChange::SendPartitionClear,
        };
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        let scheduled_event = ScheduledEvent::new(expires_at, clear_event, sequence);
        inner.event_queue.schedule(scheduled_event);

        tracing::debug!("Partitioned sends from {} until {:?}", ip, expires_at);
        Ok(())
    }

    /// Block all incoming communication to an IP address
    pub fn partition_recv_to(
        &self,
        ip: std::net::IpAddr,
        duration: Duration,
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        let expires_at = inner.current_time + duration;

        inner.network.recv_partitions.insert(ip, expires_at);

        let clear_event = Event::Connection {
            id: 0,
            state: ConnectionStateChange::RecvPartitionClear,
        };
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        let scheduled_event = ScheduledEvent::new(expires_at, clear_event, sequence);
        inner.event_queue.schedule(scheduled_event);

        tracing::debug!("Partitioned receives to {} until {:?}", ip, expires_at);
        Ok(())
    }

    /// Immediately restore communication between two IP addresses
    pub fn restore_partition(
        &self,
        from_ip: std::net::IpAddr,
        to_ip: std::net::IpAddr,
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.network.ip_partitions.remove(&(from_ip, to_ip));
        tracing::debug!("Restored partition {} -> {}", from_ip, to_ip);
        Ok(())
    }

    /// Check if communication between two IP addresses is currently partitioned
    pub fn is_partitioned(
        &self,
        from_ip: std::net::IpAddr,
        to_ip: std::net::IpAddr,
    ) -> SimulationResult<bool> {
        let inner = self.inner.borrow();
        Ok(inner
            .network
            .is_partitioned(from_ip, to_ip, inner.current_time))
    }

    /// Helper method for use with SimInner - randomly trigger partitions
    ///
    /// Supports different partition strategies based on configuration:
    /// - Random: randomly partition individual IP pairs
    /// - UniformSize: create uniform-sized partition groups
    /// - IsolateSingle: isolate exactly one node from the rest
    fn randomly_trigger_partitions_with_inner(inner: &mut SimInner) {
        let partition_config = &inner.network.config;

        if partition_config.chaos.partition_probability == 0.0 {
            return;
        }

        // Check if we should trigger a partition this step
        if sim_random::<f64>() >= partition_config.chaos.partition_probability {
            return;
        }

        // Collect unique IPs from connections
        let unique_ips: HashSet<IpAddr> = inner
            .network
            .connections
            .values()
            .filter_map(|conn| conn.local_ip)
            .collect();

        if unique_ips.len() < 2 {
            return; // Need at least 2 IPs to partition
        }

        let ip_list: Vec<IpAddr> = unique_ips.into_iter().collect();
        let partition_duration =
            crate::network::sample_duration(&partition_config.chaos.partition_duration);
        let expires_at = inner.current_time + partition_duration;

        // Select IPs to partition based on strategy
        let partitioned_ips: Vec<IpAddr> = match partition_config.chaos.partition_strategy {
            PartitionStrategy::Random => {
                // Original behavior: randomly decide for each IP
                ip_list
                    .iter()
                    .filter(|_| sim_random::<f64>() < 0.5)
                    .copied()
                    .collect()
            }
            PartitionStrategy::UniformSize => {
                // TigerBeetle-style: random partition size from 1 to n-1
                let partition_size = sim_random_range(1..ip_list.len());
                // Shuffle and take first N IPs
                let mut shuffled = ip_list.clone();
                // Simple Fisher-Yates shuffle
                for i in (1..shuffled.len()).rev() {
                    let j = sim_random_range(0..i + 1);
                    shuffled.swap(i, j);
                }
                shuffled.into_iter().take(partition_size).collect()
            }
            PartitionStrategy::IsolateSingle => {
                // Isolate exactly one node
                let idx = sim_random_range(0..ip_list.len());
                vec![ip_list[idx]]
            }
        };

        // Don't partition if we selected all IPs or none
        if partitioned_ips.is_empty() || partitioned_ips.len() == ip_list.len() {
            return;
        }

        // Create bi-directional partitions between partitioned and non-partitioned groups
        let non_partitioned: Vec<IpAddr> = ip_list
            .iter()
            .filter(|ip| !partitioned_ips.contains(ip))
            .copied()
            .collect();

        for &from_ip in &partitioned_ips {
            for &to_ip in &non_partitioned {
                // Skip if already partitioned
                if inner
                    .network
                    .is_partitioned(from_ip, to_ip, inner.current_time)
                {
                    continue;
                }

                // Partition in both directions
                inner
                    .network
                    .ip_partitions
                    .insert((from_ip, to_ip), PartitionState { expires_at });
                inner
                    .network
                    .ip_partitions
                    .insert((to_ip, from_ip), PartitionState { expires_at });

                tracing::debug!(
                    "Partition triggered: {} <-> {} until {:?} (strategy: {:?})",
                    from_ip,
                    to_ip,
                    expires_at,
                    partition_config.chaos.partition_strategy
                );
            }
        }

        // Schedule restoration event
        let restore_event = Event::Connection {
            id: 0,
            state: ConnectionStateChange::PartitionRestore,
        };
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        let scheduled_event = ScheduledEvent::new(expires_at, restore_event, sequence);
        inner.event_queue.schedule(scheduled_event);
    }

    // ==========================================================================
    // Storage Methods
    // ==========================================================================

    /// Access storage configuration for the simulation.
    pub fn with_storage_config<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&crate::storage::StorageConfiguration) -> R,
    {
        let inner = self.inner.borrow();
        f(&inner.storage.config)
    }

    /// Open a file in the simulation.
    ///
    /// Creates a new file or opens an existing one based on the options.
    /// Schedules an `OpenComplete` event and returns the file ID.
    pub(crate) fn open_file(
        &self,
        path: &str,
        options: moonpool_core::OpenOptions,
        initial_size: u64,
    ) -> SimulationResult<FileId> {
        use crate::storage::InMemoryStorage;

        let mut inner = self.inner.borrow_mut();
        let path_str = path.to_string();

        // Check create_new semantics - fail if file exists
        if options.create_new && inner.storage.path_to_file.contains_key(&path_str) {
            return Err(SimulationError::IoError(
                "File already exists (create_new)".to_string(),
            ));
        }

        // Check if file was deleted and create is not set
        if inner.storage.deleted_paths.contains(&path_str) && !options.create {
            return Err(SimulationError::IoError("File not found".to_string()));
        }

        // If file already exists and we're opening it, return existing file ID
        if let Some(&existing_id) = inner.storage.path_to_file.get(&path_str) {
            // If truncate is set, reset the storage
            if options.truncate
                && let Some(file_state) = inner.storage.files.get_mut(&existing_id)
            {
                let seed = sim_random::<u64>();
                file_state.storage = InMemoryStorage::new(0, seed);
                file_state.position = 0;
            }
            return Ok(existing_id);
        }

        // Create new file
        let file_id = FileId(inner.storage.next_file_id);
        inner.storage.next_file_id += 1;

        // Remove from deleted paths if re-creating
        inner.storage.deleted_paths.remove(&path_str);

        // Create in-memory storage with deterministic seed
        let seed = sim_random::<u64>();
        let storage = InMemoryStorage::new(initial_size, seed);

        let file_state =
            super::state::StorageFileState::new(file_id, path_str.clone(), options, storage);

        inner.storage.files.insert(file_id, file_state);
        inner.storage.path_to_file.insert(path_str, file_id);

        // Schedule OpenComplete event with minimal latency
        let open_latency = Duration::from_micros(1);
        let scheduled_time = inner.current_time + open_latency;
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        let event = Event::Storage {
            file_id: file_id.0,
            operation: StorageOperation::OpenComplete,
        };
        inner
            .event_queue
            .schedule(ScheduledEvent::new(scheduled_time, event, sequence));

        tracing::debug!("Opened file {:?} with id {:?}", path, file_id);
        Ok(file_id)
    }

    /// Check if a file exists at the given path.
    pub(crate) fn file_exists(&self, path: &str) -> bool {
        let inner = self.inner.borrow();
        let path_str = path.to_string();
        inner.storage.path_to_file.contains_key(&path_str)
            && !inner.storage.deleted_paths.contains(&path_str)
    }

    /// Delete a file at the given path.
    pub(crate) fn delete_file(&self, path: &str) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        let path_str = path.to_string();

        if let Some(file_id) = inner.storage.path_to_file.remove(&path_str) {
            // Mark file as closed and remove it
            if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
                file_state.is_closed = true;
            }
            inner.storage.files.remove(&file_id);
            inner.storage.deleted_paths.insert(path_str);
            tracing::debug!("Deleted file {:?}", path);
            Ok(())
        } else {
            Err(SimulationError::IoError("File not found".to_string()))
        }
    }

    /// Rename a file from one path to another.
    pub(crate) fn rename_file(&self, from: &str, to: &str) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        let from_str = from.to_string();
        let to_str = to.to_string();

        if let Some(file_id) = inner.storage.path_to_file.remove(&from_str) {
            // Update the path in the file state
            if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
                file_state.path = to_str.clone();
            }
            inner.storage.path_to_file.insert(to_str, file_id);
            inner.storage.deleted_paths.remove(&from_str);
            tracing::debug!("Renamed file {:?} to {:?}", from, to);
            Ok(())
        } else {
            Err(SimulationError::IoError("File not found".to_string()))
        }
    }

    /// Close a file by its ID.
    pub(crate) fn close_file(&self, file_id: FileId) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();

        if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
            file_state.is_closed = true;
            tracing::debug!("Closed file {:?}", file_id);
            Ok(())
        } else {
            Err(SimulationError::IoError("File not found".to_string()))
        }
    }

    /// Schedule a read operation on a file.
    ///
    /// Returns an operation sequence number that can be used to check completion.
    pub(crate) fn schedule_read(
        &self,
        file_id: FileId,
        offset: u64,
        len: usize,
    ) -> SimulationResult<u64> {
        let mut inner = self.inner.borrow_mut();

        let file_state = inner
            .storage
            .files
            .get_mut(&file_id)
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))?;

        if file_state.is_closed {
            return Err(SimulationError::IoError("File is closed".to_string()));
        }

        let op_seq = file_state.next_op_seq;
        file_state.next_op_seq += 1;

        // Store the pending operation
        file_state.pending_ops.insert(
            op_seq,
            PendingStorageOp {
                op_type: PendingOpType::Read,
                offset,
                len,
                data: None,
            },
        );

        // Calculate latency and schedule completion event
        let latency = Self::calculate_storage_latency(&inner.storage.config, len, false);
        let scheduled_time = inner.current_time + latency;
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        let event = Event::Storage {
            file_id: file_id.0,
            operation: StorageOperation::ReadComplete { len: len as u32 },
        };
        inner
            .event_queue
            .schedule(ScheduledEvent::new(scheduled_time, event, sequence));

        tracing::trace!(
            "Scheduled read: file={:?}, offset={}, len={}, op_seq={}",
            file_id,
            offset,
            len,
            op_seq
        );

        Ok(op_seq)
    }

    /// Schedule a write operation on a file.
    ///
    /// Returns an operation sequence number that can be used to check completion.
    pub(crate) fn schedule_write(
        &self,
        file_id: FileId,
        offset: u64,
        data: Vec<u8>,
    ) -> SimulationResult<u64> {
        let mut inner = self.inner.borrow_mut();

        let file_state = inner
            .storage
            .files
            .get_mut(&file_id)
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))?;

        if file_state.is_closed {
            return Err(SimulationError::IoError("File is closed".to_string()));
        }

        let op_seq = file_state.next_op_seq;
        file_state.next_op_seq += 1;
        let len = data.len();

        // Store the pending operation with the data
        file_state.pending_ops.insert(
            op_seq,
            PendingStorageOp {
                op_type: PendingOpType::Write,
                offset,
                len,
                data: Some(data),
            },
        );

        // Calculate latency and schedule completion event
        let latency = Self::calculate_storage_latency(&inner.storage.config, len, true);
        let scheduled_time = inner.current_time + latency;
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        let event = Event::Storage {
            file_id: file_id.0,
            operation: StorageOperation::WriteComplete { len: len as u32 },
        };
        inner
            .event_queue
            .schedule(ScheduledEvent::new(scheduled_time, event, sequence));

        tracing::trace!(
            "Scheduled write: file={:?}, offset={}, len={}, op_seq={}",
            file_id,
            offset,
            len,
            op_seq
        );

        Ok(op_seq)
    }

    /// Schedule a sync operation on a file.
    ///
    /// Returns an operation sequence number that can be used to check completion.
    pub(crate) fn schedule_sync(&self, file_id: FileId) -> SimulationResult<u64> {
        let mut inner = self.inner.borrow_mut();

        let file_state = inner
            .storage
            .files
            .get_mut(&file_id)
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))?;

        if file_state.is_closed {
            return Err(SimulationError::IoError("File is closed".to_string()));
        }

        let op_seq = file_state.next_op_seq;
        file_state.next_op_seq += 1;

        // Store the pending operation
        file_state.pending_ops.insert(
            op_seq,
            PendingStorageOp {
                op_type: PendingOpType::Sync,
                offset: 0,
                len: 0,
                data: None,
            },
        );

        // Use sync latency from config
        let latency = crate::network::sample_duration(&inner.storage.config.sync_latency);
        let scheduled_time = inner.current_time + latency;
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        let event = Event::Storage {
            file_id: file_id.0,
            operation: StorageOperation::SyncComplete,
        };
        inner
            .event_queue
            .schedule(ScheduledEvent::new(scheduled_time, event, sequence));

        tracing::trace!("Scheduled sync: file={:?}, op_seq={}", file_id, op_seq);

        Ok(op_seq)
    }

    /// Schedule a set_len operation on a file.
    ///
    /// Returns an operation sequence number that can be used to check completion.
    pub(crate) fn schedule_set_len(&self, file_id: FileId, new_len: u64) -> SimulationResult<u64> {
        let mut inner = self.inner.borrow_mut();

        let file_state = inner
            .storage
            .files
            .get_mut(&file_id)
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))?;

        if file_state.is_closed {
            return Err(SimulationError::IoError("File is closed".to_string()));
        }

        let op_seq = file_state.next_op_seq;
        file_state.next_op_seq += 1;

        // Store the pending operation
        file_state.pending_ops.insert(
            op_seq,
            PendingStorageOp {
                op_type: PendingOpType::SetLen,
                offset: new_len,
                len: 0,
                data: None,
            },
        );

        // Use write latency for set_len
        let latency = crate::network::sample_duration(&inner.storage.config.write_latency);
        let scheduled_time = inner.current_time + latency;
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        let event = Event::Storage {
            file_id: file_id.0,
            operation: StorageOperation::SetLenComplete { new_len },
        };
        inner
            .event_queue
            .schedule(ScheduledEvent::new(scheduled_time, event, sequence));

        tracing::trace!(
            "Scheduled set_len: file={:?}, new_len={}, op_seq={}",
            file_id,
            new_len,
            op_seq
        );

        Ok(op_seq)
    }

    /// Check if a storage operation is complete.
    pub(crate) fn is_storage_op_complete(&self, file_id: FileId, op_seq: u64) -> bool {
        let inner = self.inner.borrow();
        if let Some(file_state) = inner.storage.files.get(&file_id) {
            // Operation is complete when it's no longer in pending_ops
            !file_state.pending_ops.contains_key(&op_seq)
        } else {
            // File not found means operation is effectively "complete" (failed)
            true
        }
    }

    /// Register a waker for a storage operation.
    pub(crate) fn register_storage_waker(&self, file_id: FileId, op_seq: u64, waker: Waker) {
        let mut inner = self.inner.borrow_mut();
        inner.wakers.storage_wakers.insert((file_id, op_seq), waker);
    }

    /// Read data from a file at the given offset.
    ///
    /// This is called after ReadComplete to actually fetch the data.
    pub(crate) fn read_from_file(
        &self,
        file_id: FileId,
        offset: u64,
        buf: &mut [u8],
    ) -> SimulationResult<usize> {
        let inner = self.inner.borrow();

        let file_state = inner
            .storage
            .files
            .get(&file_id)
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))?;

        if file_state.is_closed {
            return Err(SimulationError::IoError("File is closed".to_string()));
        }

        // Read from the in-memory storage
        file_state
            .storage
            .read(offset, buf)
            .map_err(|e| SimulationError::IoError(e.to_string()))?;

        Ok(buf.len())
    }

    /// Get the current file position.
    pub(crate) fn get_file_position(&self, file_id: FileId) -> SimulationResult<u64> {
        let inner = self.inner.borrow();
        inner
            .storage
            .files
            .get(&file_id)
            .map(|f| f.position)
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))
    }

    /// Set the current file position.
    pub(crate) fn set_file_position(&self, file_id: FileId, position: u64) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
            file_state.position = position;
            Ok(())
        } else {
            Err(SimulationError::IoError("File not found".to_string()))
        }
    }

    /// Get the size of a file.
    pub(crate) fn get_file_size(&self, file_id: FileId) -> SimulationResult<u64> {
        let inner = self.inner.borrow();
        inner
            .storage
            .files
            .get(&file_id)
            .map(|f| f.storage.size())
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))
    }

    /// Calculate storage latency using FDB formula.
    ///
    /// Latency = base_latency + iops_overhead + transfer_time
    fn calculate_storage_latency(
        config: &crate::storage::StorageConfiguration,
        size: usize,
        is_write: bool,
    ) -> Duration {
        // Sample base latency from config range
        let base_range = if is_write {
            &config.write_latency
        } else {
            &config.read_latency
        };
        let base = crate::network::sample_duration(base_range);

        // IOPS overhead: 1/iops seconds per operation
        let iops_overhead = Duration::from_secs_f64(1.0 / config.iops as f64);

        // Transfer time: size / bandwidth seconds
        let transfer = Duration::from_secs_f64(size as f64 / config.bandwidth as f64);

        base + iops_overhead + transfer
    }

    /// Simulate a crash affecting storage.
    ///
    /// This applies crash simulation to all open files:
    /// 1. Calls `apply_crash()` on all `InMemoryStorage` instances
    /// 2. Clears pending operations (lost in crash)
    /// 3. Optionally marks files as closed
    /// 4. Wakes all storage wakers (operations will fail)
    pub fn simulate_crash(&self, close_files: bool) {
        let mut inner = self.inner.borrow_mut();
        let crash_probability = inner.storage.config.crash_fault_probability;

        // Collect all wakers to wake in one pass (to avoid borrow conflict)
        let mut wakers_to_wake = Vec::new();
        let file_ids: Vec<FileId> = inner.storage.files.keys().copied().collect();

        for file_id in &file_ids {
            if let Some(file_state) = inner.storage.files.get_mut(file_id) {
                // Apply crash to in-memory storage (may corrupt pending writes)
                file_state.storage.apply_crash(crash_probability);

                // Collect lost op sequence numbers
                let lost_ops: Vec<u64> = file_state.pending_ops.keys().copied().collect();

                // Clear pending ops - they're lost in crash
                file_state.pending_ops.clear();

                // Collect waker keys for later removal
                for op_seq in lost_ops {
                    wakers_to_wake.push((*file_id, op_seq));
                }

                // Optionally close files
                if close_files {
                    file_state.is_closed = true;
                }
            }
        }

        // Wake all collected wakers (after file iteration is complete)
        for key in wakers_to_wake {
            if let Some(waker) = inner.wakers.storage_wakers.remove(&key) {
                waker.wake();
            }
        }

        tracing::info!(
            "Storage crash simulated: {} files affected, close_files={}",
            file_ids.len(),
            close_files
        );
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
/// a strong reference that would prevent cleanup.
#[derive(Debug)]
pub struct WeakSimWorld {
    pub(crate) inner: Weak<RefCell<SimInner>>,
}

/// Macro to generate WeakSimWorld forwarding methods that wrap SimWorld results.
macro_rules! weak_forward {
    // For methods returning T that need Ok() wrapping
    (wrap $(#[$meta:meta])* $method:ident(&self $(, $arg:ident : $arg_ty:ty)*) -> $ret:ty) => {
        $(#[$meta])*
        pub fn $method(&self $(, $arg: $arg_ty)*) -> SimulationResult<$ret> {
            Ok(self.upgrade()?.$method($($arg),*))
        }
    };
    // For methods already returning SimulationResult
    (pass $(#[$meta:meta])* $method:ident(&self $(, $arg:ident : $arg_ty:ty)*) -> $ret:ty) => {
        $(#[$meta])*
        pub fn $method(&self $(, $arg: $arg_ty)*) -> SimulationResult<$ret> {
            self.upgrade()?.$method($($arg),*)
        }
    };
    // For methods returning () that need Ok(()) wrapping
    (unit $(#[$meta:meta])* $method:ident(&self $(, $arg:ident : $arg_ty:ty)*)) => {
        $(#[$meta])*
        pub fn $method(&self $(, $arg: $arg_ty)*) -> SimulationResult<()> {
            self.upgrade()?.$method($($arg),*);
            Ok(())
        }
    };
}

impl WeakSimWorld {
    /// Attempts to upgrade this weak reference to a strong reference.
    pub fn upgrade(&self) -> SimulationResult<SimWorld> {
        self.inner
            .upgrade()
            .map(|inner| SimWorld { inner })
            .ok_or(SimulationError::SimulationShutdown)
    }

    weak_forward!(wrap #[doc = "Returns the current simulation time."] current_time(&self) -> Duration);
    weak_forward!(wrap #[doc = "Returns the exact simulation time (equivalent to FDB's now())."] now(&self) -> Duration);
    weak_forward!(wrap #[doc = "Returns the drifted timer time (equivalent to FDB's timer())."] timer(&self) -> Duration);
    weak_forward!(unit #[doc = "Schedules an event to execute after the specified delay."] schedule_event(&self, event: Event, delay: Duration));
    weak_forward!(unit #[doc = "Schedules an event to execute at the specified absolute time."] schedule_event_at(&self, event: Event, time: Duration));
    weak_forward!(pass #[doc = "Read data from connection's receive buffer."] read_from_connection(&self, connection_id: ConnectionId, buf: &mut [u8]) -> usize);
    weak_forward!(pass #[doc = "Write data to connection's receive buffer."] write_to_connection(&self, connection_id: ConnectionId, data: &[u8]) -> ());
    weak_forward!(pass #[doc = "Buffer data for ordered sending on a connection."] buffer_send(&self, connection_id: ConnectionId, data: Vec<u8>) -> ());
    weak_forward!(wrap #[doc = "Get a network provider for the simulation."] network_provider(&self) -> SimNetworkProvider);
    weak_forward!(wrap #[doc = "Get a time provider for the simulation."] time_provider(&self) -> crate::providers::SimTimeProvider);
    weak_forward!(wrap #[doc = "Sleep for the specified duration in simulation time."] sleep(&self, duration: Duration) -> SleepFuture);

    /// Access network configuration for latency calculations.
    pub fn with_network_config<F, R>(&self, f: F) -> SimulationResult<R>
    where
        F: FnOnce(&NetworkConfiguration) -> R,
    {
        Ok(self.upgrade()?.with_network_config(f))
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
        sim.schedule_event(Event::Timer { task_id: 1 }, Duration::from_millis(100));

        assert!(sim.has_pending_events());
        assert_eq!(sim.pending_event_count(), 1);
        assert_eq!(sim.current_time(), Duration::ZERO);

        // Process the event
        let has_more = sim.step();
        assert!(!has_more);
        assert_eq!(sim.current_time(), Duration::from_millis(100));
        assert!(!sim.has_pending_events());
        assert_eq!(sim.pending_event_count(), 0);
    }

    #[test]
    fn sim_world_multiple_events() {
        let mut sim = SimWorld::new();

        // Schedule multiple events
        sim.schedule_event(Event::Timer { task_id: 3 }, Duration::from_millis(300));
        sim.schedule_event(Event::Timer { task_id: 1 }, Duration::from_millis(100));
        sim.schedule_event(Event::Timer { task_id: 2 }, Duration::from_millis(200));

        assert_eq!(sim.pending_event_count(), 3);

        // Process events - should happen in time order
        assert!(sim.step());
        assert_eq!(sim.current_time(), Duration::from_millis(100));
        assert_eq!(sim.pending_event_count(), 2);

        assert!(sim.step());
        assert_eq!(sim.current_time(), Duration::from_millis(200));
        assert_eq!(sim.pending_event_count(), 1);

        assert!(!sim.step());
        assert_eq!(sim.current_time(), Duration::from_millis(300));
        assert_eq!(sim.pending_event_count(), 0);
    }

    #[test]
    fn sim_world_run_until_empty() {
        let mut sim = SimWorld::new();

        // Schedule multiple events
        sim.schedule_event(Event::Timer { task_id: 1 }, Duration::from_millis(100));
        sim.schedule_event(Event::Timer { task_id: 2 }, Duration::from_millis(200));
        sim.schedule_event(Event::Timer { task_id: 3 }, Duration::from_millis(300));

        // Run until all events are processed
        sim.run_until_empty();

        assert_eq!(sim.current_time(), Duration::from_millis(300));
        assert!(!sim.has_pending_events());
    }

    #[test]
    fn sim_world_schedule_at_specific_time() {
        let mut sim = SimWorld::new();

        // Schedule event at specific time
        sim.schedule_event_at(Event::Timer { task_id: 1 }, Duration::from_millis(500));

        assert_eq!(sim.current_time(), Duration::ZERO);

        sim.step();

        assert_eq!(sim.current_time(), Duration::from_millis(500));
    }

    #[test]
    fn weak_sim_world_lifecycle() {
        let sim = SimWorld::new();
        let weak = sim.downgrade();

        // Can upgrade and use weak reference
        assert_eq!(
            weak.current_time().expect("should get time"),
            Duration::ZERO
        );

        // Schedule event through weak reference
        weak.schedule_event(Event::Timer { task_id: 1 }, Duration::from_millis(100))
            .expect("should schedule event");

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
            weak.schedule_event(Event::Timer { task_id: 2 }, Duration::from_millis(200)),
            Err(SimulationError::SimulationShutdown)
        );
    }

    #[test]
    fn deterministic_event_ordering() {
        let mut sim = SimWorld::new();

        // Schedule events at the same time
        sim.schedule_event(Event::Timer { task_id: 2 }, Duration::from_millis(100));
        sim.schedule_event(Event::Timer { task_id: 1 }, Duration::from_millis(100));
        sim.schedule_event(Event::Timer { task_id: 3 }, Duration::from_millis(100));

        // All events are at the same time, but should be processed in sequence order
        assert!(sim.step());
        assert_eq!(sim.current_time(), Duration::from_millis(100));
        assert!(sim.step());
        assert_eq!(sim.current_time(), Duration::from_millis(100));
        assert!(!sim.step());
        assert_eq!(sim.current_time(), Duration::from_millis(100));
    }
}
