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
    error::{SimulationError, SimulationResult},
    network::{
        NetworkConfiguration,
        sim::{ConnectionId, ListenerId, SimNetworkProvider},
    },
};

use super::{
    events::{ConnectionStateChange, Event, EventQueue, NetworkOperation, ScheduledEvent},
    rng::{reset_sim_rng, set_sim_seed, sim_random, sim_random_range},
    sleep::SleepFuture,
    state::{ClogState, CloseReason, ConnectionState, ListenerState, NetworkState, PartitionState},
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
    /// Creates a new simulation world with default network configuration.
    ///
    /// Uses default seed (0) for reproducible testing. For custom seeds,
    /// use [`SimWorld::new_with_seed`].
    pub fn new() -> Self {
        // Initialize with default seed for deterministic behavior
        reset_sim_rng();
        set_sim_seed(0);
        crate::chaos::assertions::reset_assertion_results();

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
    /// Create simulation with specific seed for reproducible behavior.
    pub fn new_with_seed(seed: u64) -> Self {
        reset_sim_rng();
        set_sim_seed(seed);
        crate::chaos::assertions::reset_assertion_results();

        Self {
            inner: Rc::new(RefCell::new(SimInner::new())),
        }
    }

    /// Creates a new simulation world with custom network configuration.
    pub fn new_with_network_config(network_config: NetworkConfiguration) -> Self {
        // Initialize with default seed for deterministic behavior
        reset_sim_rng();
        set_sim_seed(0);
        crate::chaos::assertions::reset_assertion_results();

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
    /// Create simulation with specific network configuration and seed.
    pub fn new_with_network_config_and_seed(
        network_config: NetworkConfiguration,
        seed: u64,
    ) -> Self {
        reset_sim_rng();
        set_sim_seed(seed);
        crate::chaos::assertions::reset_assertion_results();

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
    pub fn time_provider(&self) -> crate::time::SimTimeProvider {
        crate::time::SimTimeProvider::new(self.downgrade())
    }

    /// Create a task provider for this simulation
    pub fn task_provider(&self) -> crate::task::TokioTaskProvider {
        crate::task::TokioTaskProvider
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
        // Increment event processing counter for metrics
        inner.events_processed += 1;

        // Process different event types
        match event {
            Event::Timer { task_id } => {
                // Mark this task as awakened
                inner.awakened_tasks.insert(task_id);

                // Wake the future that was sleeping
                if let Some(waker) = inner.wakers.task_wakers.remove(&task_id) {
                    waker.wake();
                }
            }
            Event::Connection { id, state } => {
                match state {
                    ConnectionStateChange::BindComplete => {
                        // Network bind completed
                    }
                    ConnectionStateChange::ConnectionReady => {
                        // Connection establishment completed
                    }
                    ConnectionStateChange::ClogClear => {
                        // Clear write clog for the specified connection
                        let connection_id = ConnectionId(id);

                        inner.network.connection_clogs.remove(&connection_id);
                        if let Some(wakers) = inner.wakers.clog_wakers.remove(&connection_id) {
                            for waker in wakers {
                                waker.wake();
                            }
                        }
                    }
                    ConnectionStateChange::ReadClogClear => {
                        // Clear read clog for the specified connection
                        let connection_id = ConnectionId(id);

                        inner.network.read_clogs.remove(&connection_id);
                        if let Some(wakers) = inner.wakers.read_clog_wakers.remove(&connection_id) {
                            for waker in wakers {
                                waker.wake();
                            }
                        }
                    }
                    ConnectionStateChange::PartitionRestore => {
                        // Clear expired IP partitions
                        let now = inner.current_time;
                        let expired_partitions: Vec<_> = inner
                            .network
                            .ip_partitions
                            .iter()
                            .filter_map(|(pair, state)| {
                                if now >= state.expires_at {
                                    Some(*pair)
                                } else {
                                    None
                                }
                            })
                            .collect();

                        for pair in expired_partitions {
                            inner.network.ip_partitions.remove(&pair);
                            tracing::debug!("Restored IP partition {} -> {}", pair.0, pair.1);
                        }
                    }
                    ConnectionStateChange::SendPartitionClear => {
                        // Clear expired send partitions
                        let now = inner.current_time;
                        let expired_ips: Vec<_> = inner
                            .network
                            .send_partitions
                            .iter()
                            .filter_map(
                                |(ip, &expires_at)| {
                                    if now >= expires_at { Some(*ip) } else { None }
                                },
                            )
                            .collect();

                        for ip in expired_ips {
                            inner.network.send_partitions.remove(&ip);
                            tracing::debug!("Cleared send partition for {}", ip);
                        }
                    }
                    ConnectionStateChange::RecvPartitionClear => {
                        // Clear expired receive partitions
                        let now = inner.current_time;
                        let expired_ips: Vec<_> = inner
                            .network
                            .recv_partitions
                            .iter()
                            .filter_map(
                                |(ip, &expires_at)| {
                                    if now >= expires_at { Some(*ip) } else { None }
                                },
                            )
                            .collect();

                        for ip in expired_ips {
                            inner.network.recv_partitions.remove(&ip);
                            tracing::debug!("Cleared receive partition for {}", ip);
                        }
                    }
                    ConnectionStateChange::CutRestore => {
                        // Restore a temporarily cut connection
                        let connection_id = ConnectionId(id);

                        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
                            if conn.is_cut {
                                conn.is_cut = false;
                                conn.cut_expiry = None;
                                tracing::debug!(
                                    "Connection {} restored via scheduled event",
                                    connection_id.0
                                );

                                // Wake any tasks waiting for restoration
                                if let Some(wakers) = inner.wakers.cut_wakers.remove(&connection_id)
                                {
                                    for waker in wakers {
                                        waker.wake();
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Event::Network {
                connection_id,
                operation,
            } => {
                match operation {
                    NetworkOperation::DataDelivery { data } => {
                        let data_preview =
                            String::from_utf8_lossy(&data[..std::cmp::min(data.len(), 20)]);
                        tracing::info!(
                            "Event::DataDelivery processing delivery of {} bytes: '{}' to connection {}",
                            data.len(),
                            data_preview,
                            connection_id
                        );

                        let connection_id = ConnectionId(connection_id);

                        // Write data to the specified connection's receive buffer
                        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
                            // Apply bit flipping chaos BEFORE writing to receive buffer
                            let data_to_deliver = if data.is_empty() {
                                data
                            } else {
                                let chaos = &inner.network.config.chaos;
                                let now = inner.current_time;
                                let cooldown_elapsed = now.saturating_sub(inner.last_bit_flip_time)
                                    >= chaos.bit_flip_cooldown;

                                if cooldown_elapsed
                                    && crate::buggify_with_prob!(chaos.bit_flip_probability)
                                {
                                    let random_value = sim_random::<u32>();
                                    let flip_count = SimInner::calculate_flip_bit_count(
                                        random_value,
                                        chaos.bit_flip_min_bits,
                                        chaos.bit_flip_max_bits,
                                    );

                                    let mut corrupted_data = data.clone();
                                    let mut flipped_positions = std::collections::HashSet::new();
                                    for _ in 0..flip_count {
                                        let byte_idx =
                                            (sim_random::<u64>() as usize) % corrupted_data.len();
                                        let bit_idx = (sim_random::<u64>() as usize) % 8;

                                        let position = (byte_idx, bit_idx);
                                        if !flipped_positions.contains(&position) {
                                            flipped_positions.insert(position);
                                            corrupted_data[byte_idx] ^= 1 << bit_idx;
                                        }
                                    }

                                    inner.last_bit_flip_time = now;

                                    tracing::info!(
                                        "BitFlipInjected: connection={} bytes={} bits_flipped={} unique_positions={}",
                                        connection_id.0,
                                        data.len(),
                                        flip_count,
                                        flipped_positions.len()
                                    );

                                    corrupted_data
                                } else {
                                    data
                                }
                            };

                            tracing::debug!(
                                "DataDelivery writing {} bytes to connection {} receive_buffer",
                                data_to_deliver.len(),
                                connection_id.0
                            );
                            for &byte in &data_to_deliver {
                                conn.receive_buffer.push_back(byte);
                            }

                            if let Some(waker) = inner.wakers.read_wakers.remove(&connection_id) {
                                tracing::debug!(
                                    "DataDelivery waking up read waker for connection_id={}",
                                    connection_id.0
                                );
                                waker.wake();
                            } else {
                                tracing::debug!(
                                    "DataDelivery no waker found for connection_id={}",
                                    connection_id.0
                                );
                            }
                        } else {
                            tracing::warn!(
                                "DataDelivery failed: connection_id={} not found in connections HashMap",
                                connection_id.0
                            );
                        }
                    }
                    NetworkOperation::ProcessSendBuffer => {
                        let connection_id = ConnectionId(connection_id);

                        // Check if this connection is partitioned before processing
                        let is_partitioned = inner
                            .network
                            .is_connection_partitioned(connection_id, inner.current_time);

                        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
                            if is_partitioned {
                                if let Some(data) = conn.send_buffer.pop_front() {
                                    tracing::debug!(
                                        "Connection {} partitioned, failing send of {} bytes",
                                        connection_id.0,
                                        data.len()
                                    );

                                    // Wake any tasks waiting for send buffer space
                                    Self::wake_all(
                                        &mut inner.wakers.send_buffer_wakers,
                                        connection_id,
                                    );

                                    if !conn.send_buffer.is_empty() {
                                        let scheduled_time = inner.current_time;
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
                                    } else {
                                        conn.send_in_progress = false;
                                    }
                                } else {
                                    conn.send_in_progress = false;
                                }
                            } else if let Some(conn) = inner.network.connections.get(&connection_id)
                            {
                                // Extract data we need before mutable operations
                                let paired_id = conn.paired_connection;
                                let send_delay = conn.send_delay;
                                let next_send_time = conn.next_send_time;
                                let has_data = !conn.send_buffer.is_empty();

                                // Get receiver's recv_delay if paired connection exists
                                let recv_delay = paired_id.and_then(|pid| {
                                    inner
                                        .network
                                        .connections
                                        .get(&pid)
                                        .and_then(|paired_conn| paired_conn.recv_delay)
                                });

                                // Now get mutable reference and pop data
                                if has_data {
                                    if let Some(conn) =
                                        inner.network.connections.get_mut(&connection_id)
                                    {
                                        if let Some(mut data) = conn.send_buffer.pop_front() {
                                            // Wake any tasks waiting for send buffer space
                                            Self::wake_all(
                                                &mut inner.wakers.send_buffer_wakers,
                                                connection_id,
                                            );

                                            // BUGGIFY: Simulate partial/short writes
                                            if crate::buggify!()
                                                && !is_partitioned
                                                && !data.is_empty()
                                            {
                                                let max_send = std::cmp::min(
                                                    data.len(),
                                                    inner
                                                        .network
                                                        .config
                                                        .chaos
                                                        .partial_write_max_bytes,
                                                );
                                                let truncate_to = sim_random_range(0..max_send + 1);

                                                if truncate_to < data.len() {
                                                    let remainder = data.split_off(truncate_to);
                                                    conn.send_buffer.push_front(remainder);

                                                    tracing::debug!(
                                                        "BUGGIFY: Partial write on connection {} - sending {} bytes, {} bytes remain queued",
                                                        connection_id.0,
                                                        data.len(),
                                                        conn.send_buffer
                                                            .front()
                                                            .map(|v| v.len())
                                                            .unwrap_or(0)
                                                    );
                                                }
                                            }

                                            let has_more_messages = !conn.send_buffer.is_empty();
                                            let base_delay = if has_more_messages {
                                                std::time::Duration::from_nanos(1)
                                            } else {
                                                // Use per-connection send delay if set, otherwise global config
                                                send_delay.unwrap_or_else(|| {
                                                    crate::network::config::sample_duration(
                                                        &inner.network.config.write_latency,
                                                    )
                                                })
                                            };

                                            let earliest_time = std::cmp::max(
                                                inner.current_time + base_delay,
                                                next_send_time,
                                            );
                                            let actual_delay = earliest_time - inner.current_time;

                                            conn.next_send_time =
                                                earliest_time + std::time::Duration::from_nanos(1);

                                            tracing::info!(
                                                "Event::ProcessSendBuffer processing {} bytes from connection {} with delay {:?}, has_more_messages={} (TCP ordering: earliest_time={:?})",
                                                data.len(),
                                                connection_id.0,
                                                actual_delay,
                                                has_more_messages,
                                                earliest_time
                                            );

                                            if let Some(paired_id) = paired_id {
                                                // Add receiver's recv_delay if set
                                                let scheduled_time = earliest_time
                                                    + recv_delay.unwrap_or(Duration::ZERO);
                                                let sequence = inner.next_sequence;
                                                inner.next_sequence += 1;

                                                let scheduled_event = ScheduledEvent::new(
                                                    scheduled_time,
                                                    Event::Network {
                                                        connection_id: paired_id.0,
                                                        operation: NetworkOperation::DataDelivery {
                                                            data,
                                                        },
                                                    },
                                                    sequence,
                                                );
                                                inner.event_queue.schedule(scheduled_event);
                                            }

                                            if !conn.send_buffer.is_empty() {
                                                let scheduled_time = inner.current_time;
                                                let sequence = inner.next_sequence;
                                                inner.next_sequence += 1;

                                                let scheduled_event = ScheduledEvent::new(
                                                    scheduled_time,
                                                    Event::Network {
                                                        connection_id: connection_id.0,
                                                        operation:
                                                            NetworkOperation::ProcessSendBuffer,
                                                    },
                                                    sequence,
                                                );
                                                inner.event_queue.schedule(scheduled_event);
                                            } else {
                                                conn.send_in_progress = false;
                                            }
                                        } else {
                                            conn.send_in_progress = false;
                                        }
                                    }
                                } else {
                                    // No data in buffer
                                    if let Some(conn) =
                                        inner.network.connections.get_mut(&connection_id)
                                    {
                                        conn.send_in_progress = false;
                                    }
                                }
                            } else {
                                // Connection not found
                            }
                        }
                    }
                }
            }
            Event::Shutdown => {
                tracing::debug!("Processing Shutdown event - waking all pending tasks");

                let task_wakers: Vec<_> = inner.wakers.task_wakers.drain().collect();

                for (task_id, waker) in task_wakers {
                    tracing::trace!("Waking task {}", task_id);
                    waker.wake();
                }

                let read_wakers: Vec<_> = inner
                    .wakers
                    .read_wakers
                    .drain()
                    .map(|(_conn_id, waker)| waker)
                    .collect();

                for waker in read_wakers {
                    waker.wake();
                }

                tracing::debug!("Shutdown event processed - woke all pending tasks");
            }
        }
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

        let clog_duration = crate::network::config::sample_duration(&config.chaos.clog_duration);
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

        let clog_duration = crate::network::config::sample_duration(&config.chaos.clog_duration);
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

        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            if conn.is_cut {
                conn.is_cut = false;
                conn.cut_expiry = None;
                tracing::debug!("Connection {} restored", connection_id.0);

                // Wake any tasks waiting for restoration
                Self::wake_all(&mut inner.wakers.cut_wakers, connection_id);
            }
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
    pub fn set_pair_latency_if_not_set(&self, src: IpAddr, dst: IpAddr, latency: Duration) -> Duration {
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
            .unwrap_or_else(|| (IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)));
        drop(inner);

        // Check if latency is already set
        if let Some(latency) = self.get_pair_latency(local_ip, remote_ip) {
            return latency;
        }

        // Sample a new latency from config and set it
        let latency = self.with_network_config(|config| {
            crate::network::config::sample_duration(&config.write_latency)
        });
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
    /// This is the default close behavior used by `close_connection`.
    pub fn close_connection(&self, connection_id: ConnectionId) {
        self.close_connection_with_reason(connection_id, CloseReason::Graceful);
    }

    /// Close a connection gracefully (FIN semantics).
    ///
    /// Alias for `close_connection` - peer will receive EOF on read.
    pub fn close_connection_graceful(&self, connection_id: ConnectionId) {
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
    fn randomly_trigger_partitions_with_inner(inner: &mut SimInner) {
        let partition_config = &inner.network.config;

        if partition_config.chaos.partition_probability == 0.0 {
            return;
        }

        let ip_pairs: Vec<(std::net::IpAddr, std::net::IpAddr)> = inner
            .network
            .connections
            .values()
            .filter_map(|conn| {
                if let (Some(local), Some(remote)) = (conn.local_ip, conn.remote_ip) {
                    Some((local, remote))
                } else {
                    None
                }
            })
            .collect();

        for (from_ip, to_ip) in ip_pairs {
            if inner
                .network
                .is_partitioned(from_ip, to_ip, inner.current_time)
            {
                continue;
            }

            if sim_random::<f64>() < partition_config.chaos.partition_probability {
                let partition_duration = crate::network::config::sample_duration(
                    &partition_config.chaos.partition_duration,
                );
                let expires_at = inner.current_time + partition_duration;

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
                    "Randomly triggered partition {} -> {} until {:?}, restoration event scheduled with sequence {}",
                    from_ip,
                    to_ip,
                    expires_at,
                    sequence
                );
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
/// a strong reference that would prevent cleanup.
#[derive(Debug)]
pub struct WeakSimWorld {
    pub(crate) inner: Weak<RefCell<SimInner>>,
}

impl WeakSimWorld {
    /// Attempts to upgrade this weak reference to a strong reference.
    pub fn upgrade(&self) -> SimulationResult<SimWorld> {
        self.inner
            .upgrade()
            .map(|inner| SimWorld { inner })
            .ok_or(SimulationError::SimulationShutdown)
    }

    /// Returns the current simulation time.
    pub fn current_time(&self) -> SimulationResult<Duration> {
        let sim = self.upgrade()?;
        Ok(sim.current_time())
    }

    /// Returns the exact simulation time (equivalent to FDB's now()).
    pub fn now(&self) -> SimulationResult<Duration> {
        let sim = self.upgrade()?;
        Ok(sim.now())
    }

    /// Returns the drifted timer time (equivalent to FDB's timer()).
    pub fn timer(&self) -> SimulationResult<Duration> {
        let sim = self.upgrade()?;
        Ok(sim.timer())
    }

    /// Schedules an event to execute after the specified delay.
    pub fn schedule_event(&self, event: Event, delay: Duration) -> SimulationResult<()> {
        let sim = self.upgrade()?;
        sim.schedule_event(event, delay);
        Ok(())
    }

    /// Schedules an event to execute at the specified absolute time.
    pub fn schedule_event_at(&self, event: Event, time: Duration) -> SimulationResult<()> {
        let sim = self.upgrade()?;
        sim.schedule_event_at(event, time);
        Ok(())
    }

    /// Access network configuration for latency calculations.
    pub fn with_network_config<F, R>(&self, f: F) -> SimulationResult<R>
    where
        F: FnOnce(&NetworkConfiguration) -> R,
    {
        let sim = self.upgrade()?;
        Ok(sim.with_network_config(f))
    }

    /// Read data from connection's receive buffer
    pub fn read_from_connection(
        &self,
        connection_id: ConnectionId,
        buf: &mut [u8],
    ) -> SimulationResult<usize> {
        let sim = self.upgrade()?;
        sim.read_from_connection(connection_id, buf)
    }

    /// Write data to connection's receive buffer
    pub fn write_to_connection(
        &self,
        connection_id: ConnectionId,
        data: &[u8],
    ) -> SimulationResult<()> {
        let sim = self.upgrade()?;
        sim.write_to_connection(connection_id, data)
    }

    /// Buffer data for ordered sending on a connection.
    pub fn buffer_send(&self, connection_id: ConnectionId, data: Vec<u8>) -> SimulationResult<()> {
        let sim = self.upgrade()?;
        sim.buffer_send(connection_id, data)
    }

    /// Get a network provider for the simulation.
    pub fn network_provider(&self) -> SimulationResult<SimNetworkProvider> {
        let sim = self.upgrade()?;
        Ok(sim.network_provider())
    }

    /// Get a time provider for the simulation.
    pub fn time_provider(&self) -> SimulationResult<crate::time::SimTimeProvider> {
        let sim = self.upgrade()?;
        Ok(sim.time_provider())
    }

    /// Sleep for the specified duration in simulation time.
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
        assert_eq!(weak.current_time().unwrap(), Duration::ZERO);

        // Schedule event through weak reference
        weak.schedule_event(Event::Timer { task_id: 1 }, Duration::from_millis(100))
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
