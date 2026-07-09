//! Core simulation world and coordination logic.
//!
//! This module provides the central `SimWorld` coordinator that manages time,
//! event processing, and network simulation state.

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    net::IpAddr,
    sync::{Arc, RwLock, Weak},
    task::Waker,
    time::Duration,
};
use tracing::instrument;

use crate::{
    SimulationError, SimulationResult,
    chaos::fault_events::SimFaultEvent,
    network::{
        NetworkConfiguration, PartitionStrategy,
        sim::{ConnectionId, ListenerId, SimNetworkProvider},
    },
};

use super::{
    events::{ConnectionStateChange, Event, EventQueue, NetworkOperation, ScheduledEvent},
    rng::{reset_sim_rng, set_sim_seed, sim_random, sim_random_range},
    sleep::SleepFuture,
    state::{
        ClogState, CloseReason, ConnectionFlags, ConnectionState, NetworkState, PartitionState,
        StorageState,
    },
    wakers::WakerRegistry,
};

/// Internal simulation state holder
#[derive(Debug)]
pub(crate) struct SimInner {
    pub(crate) current_time: Duration,
    /// Drifted timer time (can be ahead of `current_time`)
    /// FDB ref: sim2.actor.cpp:1058-1064 - `timer()` drifts 0-0.1s ahead of `now()`
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

    // Last event processed by step() — used by orchestrator to detect ProcessRestart
    pub(crate) last_processed_event: Option<Event>,

    /// Faults recorded by the engine since the last [`SimWorld::take_faults`]
    /// drain. The runner pumps these into the observability timeline.
    pub(crate) pending_faults: Vec<SimFaultRecord>,
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
            last_processed_event: None,
            pending_faults: Vec::new(),
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
            last_processed_event: None,
            pending_faults: Vec::new(),
        }
    }

    /// Record an engine-injected fault, stamped with the current sim time.
    ///
    /// Faults accumulate in [`SimInner::pending_faults`] until the runner
    /// drains them via [`SimWorld::take_faults`] into the observability
    /// timeline. The engine never touches `tracing` for faults.
    pub(crate) fn record_fault(&mut self, event: SimFaultEvent) {
        let time_ms = u64::try_from(self.current_time.as_millis()).unwrap_or(u64::MAX);
        self.pending_faults.push(SimFaultRecord { time_ms, event });
    }

    /// Calculate the number of bits to flip using a power-law distribution.
    ///
    /// Uses the formula: 32 - `floor(log2(random_value))`
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

/// A fault recorded by the simulation engine, stamped with the sim time at
/// which it occurred.
///
/// Drained by the runner via [`SimWorld::take_faults`] and recorded into the
/// observability timeline under
/// [`crate::SIM_FAULT_EVENT_NAME`](crate::chaos::SIM_FAULT_EVENT_NAME).
#[derive(Debug, Clone)]
pub struct SimFaultRecord {
    /// Simulation time in milliseconds when the fault occurred.
    pub time_ms: u64,
    /// The fault event.
    pub event: SimFaultEvent,
}

/// The central simulation coordinator that manages time and event processing.
///
/// `SimWorld` owns all mutable simulation state and provides the main interface
/// for scheduling events and advancing simulation time. It uses a centralized
/// ownership model with handle-based access to avoid borrow checker conflicts.
#[derive(Debug)]
pub struct SimWorld {
    pub(crate) inner: Arc<RwLock<SimInner>>,
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
            inner: Arc::new(RwLock::new(inner)),
        }
    }

    /// Creates a new simulation world with default network configuration.
    ///
    /// Uses default seed (0) for reproducible testing. For custom seeds,
    /// use [`SimWorld::new_with_seed`].
    #[must_use]
    pub fn new() -> Self {
        Self::create(None, 0)
    }

    /// Drain engine-recorded faults accumulated since the last call.
    ///
    /// The runner calls this after each `step()` to pump faults into the
    /// observability timeline; tests can call it directly to assert on the
    /// fault sequence.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn take_faults(&self) -> Vec<SimFaultRecord> {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        std::mem::take(&mut inner.pending_faults)
    }

    /// Creates a new simulation world with a specific seed for deterministic randomness.
    ///
    /// This method ensures clean thread-local RNG state by resetting before
    /// setting the seed, making it safe for consecutive simulations on the same thread.
    ///
    /// # Parameters
    ///
    /// * `seed` - The seed value for deterministic randomness
    #[must_use]
    pub fn new_with_seed(seed: u64) -> Self {
        Self::create(None, seed)
    }

    /// Creates a new simulation world with custom network configuration.
    #[must_use]
    pub fn new_with_network_config(network_config: NetworkConfiguration) -> Self {
        Self::create(Some(network_config), 0)
    }

    /// Creates a new simulation world with both custom network configuration and seed.
    ///
    /// # Parameters
    ///
    /// * `network_config` - Network configuration for latency and fault simulation
    /// * `seed` - The seed value for deterministic randomness
    #[must_use]
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
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[instrument(skip(self))]
    pub fn step(&mut self) -> bool {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");

        if let Some(scheduled_event) = inner.event_queue.pop_earliest() {
            // Advance logical time to event timestamp
            inner.current_time = scheduled_event.time();

            // Phase 7: Clear expired clogs after time advancement
            Self::clear_expired_clogs_with_inner(&mut inner);

            // Trigger random partitions based on configuration
            Self::randomly_trigger_partitions_with_inner(&mut inner);

            // Store the event for orchestrator inspection (e.g., ProcessRestart)
            let event = scheduled_event.into_event();
            inner.last_processed_event = Some(event.clone());

            // Process the event with the mutable reference
            Self::process_event_with_inner(&mut inner, event);

            // Return true if more events are available
            !inner.event_queue.is_empty()
        } else {
            inner.last_processed_event = None;
            // No more events to process
            false
        }
    }

    /// Processes all scheduled events until the queue is empty or only infrastructure events remain.
    ///
    /// This method processes all workload-related events but stops early if only infrastructure
    /// events (like connection restoration) remain. This prevents infinite loops where
    /// infrastructure events keep the simulation running indefinitely after workloads complete.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[instrument(skip(self))]
    pub fn run_until_empty(&mut self) {
        while self.step() {
            // Periodically check if we should stop early (every 50 events for performance)
            if self
                .inner
                .read()
                .expect("RwLock poisoned: prior task panicked")
                .events_processed
                .is_multiple_of(50)
            {
                let has_workload_events = !self
                    .inner
                    .read()
                    .expect("RwLock poisoned: prior task panicked")
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
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn current_time(&self) -> Duration {
        self.inner
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .current_time
    }

    /// Returns the exact simulation time (equivalent to FDB's `now()`).
    ///
    /// This is the canonical simulation time used for scheduling events.
    /// Use this for precise time comparisons and scheduling.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn now(&self) -> Duration {
        self.inner
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .current_time
    }

    /// Returns the drifted timer time (equivalent to FDB's `timer()`).
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
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn timer(&self) -> Duration {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
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
            let gap = max_timer
                .checked_sub(inner.timer_time)
                .expect("timer_time < max_timer was just checked")
                .as_secs_f64();
            let delta = random_factor * gap / 2.0;
            inner.timer_time += Duration::from_secs_f64(delta);
        }

        // Ensure timer never goes backwards relative to simulation time
        inner.timer_time = inner.timer_time.max(inner.current_time);

        inner.timer_time
    }

    /// Schedules an event to execute after the specified delay from the current time.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[instrument(skip(self))]
    pub fn schedule_event(&self, event: Event, delay: Duration) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        let scheduled_time = inner.current_time + delay;
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        let scheduled_event = ScheduledEvent::new(scheduled_time, event, sequence);
        inner.event_queue.schedule(scheduled_event);
    }

    /// Schedules an event to execute at the specified absolute time.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn schedule_event_at(&self, event: Event, time: Duration) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        let scheduled_event = ScheduledEvent::new(time, event, sequence);
        inner.event_queue.schedule(scheduled_event);
    }

    /// Creates a weak reference to this simulation world.
    ///
    /// Weak references can be used to access the simulation without preventing
    /// it from being dropped, enabling handle-based access patterns.
    #[must_use]
    pub fn downgrade(&self) -> WeakSimWorld {
        WeakSimWorld {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Returns `true` if there are events waiting to be processed.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn has_pending_events(&self) -> bool {
        !self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .event_queue
            .is_empty()
    }

    /// Returns the number of events waiting to be processed.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn pending_event_count(&self) -> usize {
        self.inner
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .event_queue
            .len()
    }

    /// Create a network provider for this simulation
    #[must_use]
    pub fn network_provider(&self) -> SimNetworkProvider {
        SimNetworkProvider::new(self.downgrade())
    }

    /// Create a time provider for this simulation
    #[must_use]
    pub fn time_provider(&self) -> crate::providers::SimTimeProvider {
        crate::providers::SimTimeProvider::new(self.downgrade())
    }

    /// Create a task provider for this simulation
    #[must_use]
    pub fn task_provider(&self) -> crate::providers::SimTaskProvider {
        crate::providers::SimTaskProvider
    }

    /// Create a storage provider for this simulation scoped to a process IP.
    #[must_use]
    pub fn storage_provider(&self, ip: std::net::IpAddr) -> crate::storage::SimStorageProvider {
        crate::storage::SimStorageProvider::new(self.downgrade(), ip)
    }

    /// Set the default storage configuration for this simulation.
    ///
    /// Used as fallback when no per-process config is set for a given IP.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn set_storage_config(&mut self, config: crate::storage::StorageConfiguration) {
        self.inner
            .write()
            .expect("RwLock poisoned: prior task panicked")
            .storage
            .config = config;
    }

    /// Set the storage configuration for a single process IP.
    ///
    /// Overrides the default ([`set_storage_config`](Self::set_storage_config))
    /// for files owned by `ip`, letting different machines run with different
    /// storage timing and fault profiles in the same simulation.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn set_storage_config_for(
        &mut self,
        ip: std::net::IpAddr,
        config: crate::storage::StorageConfiguration,
    ) {
        self.inner
            .write()
            .expect("RwLock poisoned: prior task panicked")
            .storage
            .per_process_configs
            .insert(ip, config);
    }

    /// Active disk-degradation episode for a process IP, if any.
    ///
    /// Episodes are scoped per owner: one stall/throttle window applies to every
    /// file the process owns. Observability accessor for tests and diagnostics.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn disk_episode_for(
        &self,
        ip: std::net::IpAddr,
    ) -> Option<super::state::DiskDegradationState> {
        self.inner
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .storage
            .disk_episodes
            .get(&ip)
            .copied()
    }

    /// Access network configuration for latency calculations using thread-local RNG.
    ///
    /// This method provides access to the network configuration for calculating
    /// latencies and other network parameters. Random values should be generated
    /// using the thread-local RNG functions like `sim_random()`.
    ///
    /// Access the network configuration for this simulation.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn with_network_config<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&NetworkConfiguration) -> R,
    {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        f(&inner.network.config)
    }

    /// Create a listener in the simulation (used by `SimNetworkProvider`)
    pub(crate) fn create_listener(&self) -> ListenerId {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        let listener_id = ListenerId(inner.network.next_listener_id);
        inner.network.next_listener_id += 1;

        inner.network.listeners.insert(listener_id);

        listener_id
    }

    /// Read data from connection's receive buffer (used by `SimTcpStream`)
    pub(crate) fn read_from_connection(
        &self,
        connection_id: ConnectionId,
        buf: &mut [u8],
    ) -> SimulationResult<usize> {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");

        // Read the config knob before borrowing the connection mutably (disjoint
        // fields, but keeping it a local sidesteps any borrow friction).
        let partial_read_max_bytes = inner.network.config.chaos.partial_read_max_bytes;

        if let Some(connection) = inner.network.connections.get_mut(&connection_id) {
            let available = std::cmp::min(buf.len(), connection.receive_buffer.len());

            // BUGGIFY: simulate a partial read by delivering fewer bytes than are
            // available, leaving the remainder buffered for the next read (mirrors
            // FDB's Sim2Conn receiver). Skip stable connections. Always deliver at
            // least one byte so the caller never mistakes a partial read for EOF,
            // which would leave poll_read waiting on data already in the buffer.
            let limit = if available > 0 && !connection.flags.is_stable() && crate::buggify!() {
                let max_read = std::cmp::min(available, partial_read_max_bytes);
                if max_read >= 1 {
                    let n = sim_random_range(1..max_read + 1);
                    if n < available {
                        tracing::debug!(
                            "BUGGIFY: Partial read on connection {} - delivering {} of {} bytes",
                            connection_id.0,
                            n,
                            available
                        );
                    }
                    n
                } else {
                    available
                }
            } else {
                available
            };

            let mut bytes_read = 0;
            while bytes_read < limit {
                match connection.receive_buffer.pop_front() {
                    Some(byte) => {
                        buf[bytes_read] = byte;
                        bytes_read += 1;
                    }
                    None => break,
                }
            }
            Ok(bytes_read)
        } else {
            Err(SimulationError::InvalidState(
                "connection not found".to_string(),
            ))
        }
    }

    /// Write data to connection's receive buffer (used by `SimTcpStream` write operations)
    pub(crate) fn write_to_connection(
        &self,
        connection_id: ConnectionId,
        data: &[u8],
    ) -> SimulationResult<()> {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");

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
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");

        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            // Always add data to send buffer for TCP ordering
            conn.send_buffer.push_back(data);
            tracing::debug!(
                "buffer_send: added data to send_buffer, new length: {}",
                conn.send_buffer.len()
            );

            // If sender is not already active, start processing the buffer
            if conn.flags.send_in_progress() {
                tracing::debug!(
                    "buffer_send: sender already in progress, not scheduling new event"
                );
            } else {
                tracing::debug!(
                    "buffer_send: sender not in progress, scheduling ProcessSendBuffer event"
                );
                conn.flags.set_send_in_progress(true);

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
    /// - Client connection stores server's real address as `peer_address`
    /// - Server connection stores synthesized ephemeral address (random IP + port 40000-60000)
    ///
    /// This simulates real TCP behavior where servers see client ephemeral ports.
    pub(crate) fn create_connection_pair(
        &self,
        client_addr: &str,
        server_addr: &str,
    ) -> (ConnectionId, ConnectionId) {
        // Default send buffer capacity (Bandwidth-Delay Product); 64 KB is
        // typical for TCP socket buffers. Real simulations could compute
        // this as max_latency_ms * bandwidth_bytes_per_ms.
        const DEFAULT_SEND_BUFFER_CAPACITY: usize = 64 * 1024;

        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");

        let client_conn = ConnectionId(inner.network.next_connection_id);
        inner.network.next_connection_id += 1;

        let server_conn = ConnectionId(inner.network.next_connection_id);
        inner.network.next_connection_id += 1;

        // Capture current time to avoid borrow conflicts
        let current_time = inner.current_time;

        // Parse IP addresses for partition tracking
        let client_endpoint = NetworkState::parse_ip_from_addr(client_addr);
        let server_endpoint = NetworkState::parse_ip_from_addr(server_addr);

        // FDB Pattern: Synthesize ephemeral address for server-side connection
        // sim2.actor.cpp:1149-1175: randomInt(0,256) for IP offset, randomInt(40000,60000) for port
        // Use thread-local sim_random_range for deterministic randomness
        let ephemeral_peer_addr = match client_endpoint {
            Some(std::net::IpAddr::V4(ipv4)) => {
                let octets = ipv4.octets();
                let ip_offset =
                    u8::try_from(sim_random_range(0u32..256)).expect("range bounded to u8");
                let new_last_octet = octets[3].wrapping_add(ip_offset);
                let ephemeral_ip =
                    std::net::Ipv4Addr::new(octets[0], octets[1], octets[2], new_last_octet);
                let ephemeral_port = sim_random_range(40000u16..60000);
                format!("{ephemeral_ip}:{ephemeral_port}")
            }
            Some(std::net::IpAddr::V6(ipv6)) => {
                // For IPv6, just modify the last segment
                let segments = ipv6.segments();
                let mut new_segments = segments;
                let ip_offset = sim_random_range(0u16..256);
                new_segments[7] = new_segments[7].wrapping_add(ip_offset);
                let ephemeral_ip = std::net::Ipv6Addr::from(new_segments);
                let ephemeral_port = sim_random_range(40000u16..60000);
                format!("[{ephemeral_ip}]:{ephemeral_port}")
            }
            None => {
                // Fallback: use client address with random port
                let ephemeral_port = sim_random_range(40000u16..60000);
                format!("unknown:{ephemeral_port}")
            }
        };

        // Create paired connections.
        // Client stores server's real address as peer_address
        inner.network.connections.insert(
            client_conn,
            ConnectionState {
                local_ip: client_endpoint,
                remote_ip: server_endpoint,
                peer_address: server_addr.to_owned(),
                receive_buffer: VecDeque::new(),
                paired_connection: Some(server_conn),
                send_buffer: VecDeque::new(),
                next_send_time: current_time,
                flags: ConnectionFlags::default(),
                cut_expiry: None,
                close_reason: CloseReason::None,
                send_buffer_capacity: DEFAULT_SEND_BUFFER_CAPACITY,
                send_delay: None,
                recv_delay: None,
                half_open_error_at: None,
                last_data_delivery_scheduled_at: None,
            },
        );

        // Server stores synthesized ephemeral address as peer_address
        inner.network.connections.insert(
            server_conn,
            ConnectionState {
                local_ip: server_endpoint,
                remote_ip: client_endpoint,
                peer_address: ephemeral_peer_addr,
                receive_buffer: VecDeque::new(),
                paired_connection: Some(client_conn),
                send_buffer: VecDeque::new(),
                next_send_time: current_time,
                flags: ConnectionFlags::default(),
                cut_expiry: None,
                close_reason: CloseReason::None,
                send_buffer_capacity: DEFAULT_SEND_BUFFER_CAPACITY,
                send_delay: None,
                recv_delay: None,
                half_open_error_at: None,
                last_data_delivery_scheduled_at: None,
            },
        );

        (client_conn, server_conn)
    }

    /// Register a waker for read operations
    pub(crate) fn register_read_waker(&self, connection_id: ConnectionId, waker: Waker) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        let is_replacement = inner.wakers.reads.contains_key(&connection_id);
        inner.wakers.reads.insert(connection_id, waker);
        tracing::debug!(
            "register_read_waker: connection_id={}, replacement={}, total_wakers={}",
            connection_id.0,
            is_replacement,
            inner.wakers.reads.len()
        );
    }

    /// Register a waker for accept operations
    pub(crate) fn register_accept_waker(&self, addr: &str, waker: Waker) {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        // For simplicity, we'll use addr hash as listener ID for waker storage
        let mut hasher = DefaultHasher::new();
        addr.hash(&mut hasher);
        let listener_key = ListenerId(hasher.finish());

        inner.wakers.listeners.insert(listener_key, waker);
    }

    /// Store a pending connection for later `accept()` call
    pub(crate) fn store_pending_connection(&self, addr: &str, connection_id: ConnectionId) {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .network
            .pending_connections
            .insert(addr.to_string(), connection_id);

        // Wake any accept() calls waiting for this connection
        let mut hasher = DefaultHasher::new();
        addr.hash(&mut hasher);
        let listener_key = ListenerId(hasher.finish());

        if let Some(waker) = inner.wakers.listeners.remove(&listener_key) {
            waker.wake();
        }
    }

    /// Get a pending connection for `accept()` call
    pub(crate) fn pending_connection(&self, addr: &str) -> Option<ConnectionId> {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        inner.network.pending_connections.remove(addr)
    }

    /// Get the peer address for a connection.
    ///
    /// FDB Pattern (sim2.actor.cpp):
    /// - For client-side connections: returns server's listening address
    /// - For server-side connections: returns synthesized ephemeral address
    ///
    /// The returned address may not be connectable for server-side connections,
    /// matching real TCP behavior where servers see client ephemeral ports.
    pub(crate) fn connection_peer_address(&self, connection_id: ConnectionId) -> Option<String> {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
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
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
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
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        let task_id = inner.next_task_id;
        inner.next_task_id += 1;
        task_id
    }

    /// Wake all tasks associated with a connection
    fn wake_all(wakers: &mut BTreeMap<ConnectionId, Vec<Waker>>, connection_id: ConnectionId) {
        if let Some(waker_list) = wakers.remove(&connection_id) {
            for waker in waker_list {
                waker.wake();
            }
        }
    }

    /// Check if a task has been awakened.
    pub(crate) fn is_task_awake(&self, task_id: u64) -> bool {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        inner.awakened_tasks.contains(&task_id)
    }

    /// Register a waker for a task.
    pub(crate) fn register_task_waker(&self, task_id: u64, waker: Waker) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        inner.wakers.tasks.insert(task_id, waker);
    }

    /// Clear expired clogs and wake pending tasks (helper for use with `SimInner`)
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
            Self::wake_all(&mut inner.wakers.write_clogs, id);
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
                super::storage_ops::handle_storage_event(inner, file_id, operation);
            }
            Event::Shutdown => Self::handle_shutdown_event(inner),
            Event::ProcessRestart { ip }
            | Event::ProcessGracefulShutdown { ip, .. }
            | Event::ProcessForceKill { ip, .. } => {
                // Process lifecycle events are handled by the orchestrator, not SimWorld.
                // We just log them here; the orchestrator reads the event after step().
                tracing::debug!("Process lifecycle event for IP {}", ip);
            }
        }
    }

    /// Handle timer events - wake sleeping tasks.
    fn handle_timer_event(inner: &mut SimInner, task_id: u64) {
        inner.awakened_tasks.insert(task_id);
        if let Some(waker) = inner.wakers.tasks.remove(&task_id) {
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
                Self::wake_all(&mut inner.wakers.write_clogs, connection_id);
            }
            ConnectionStateChange::ReadClogClear => {
                inner.network.read_clogs.remove(&connection_id);
                Self::wake_all(&mut inner.wakers.read_clogs, connection_id);
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
                    && conn.flags.is_cut()
                {
                    conn.flags.set_is_cut(false);
                    conn.cut_expiry = None;
                    inner.record_fault(SimFaultEvent::CutRestored { connection_id: id });
                    tracing::debug!("Connection {} restored via scheduled event", id);
                    Self::wake_all(&mut inner.wakers.cuts, connection_id);
                }
            }
            ConnectionStateChange::HalfOpenError => {
                inner.record_fault(SimFaultEvent::HalfOpenError { connection_id: id });
                tracing::debug!("Connection {} half-open error time reached", id);
                if let Some(waker) = inner.wakers.reads.remove(&connection_id) {
                    waker.wake();
                }
                Self::wake_all(&mut inner.wakers.send_buffers, connection_id);
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
            NetworkOperation::FinDelivery => {
                Self::handle_fin_delivery(inner, connection_id);
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
            .is_some_and(|conn| conn.flags.is_stable());

        if !inner.network.connections.contains_key(&connection_id) {
            tracing::warn!("DataDelivery: connection {} not found", connection_id.0);
            return;
        }

        // Apply bit flipping chaos (needs mutable inner access) - skip for stable connections
        let data_to_deliver = if is_stable {
            data
        } else {
            Self::maybe_corrupt_data(inner, connection_id, &data)
        };

        // Now get connection reference and deliver data
        let Some(conn) = inner.network.connections.get_mut(&connection_id) else {
            return;
        };

        for &byte in &data_to_deliver {
            conn.receive_buffer.push_back(byte);
        }

        if let Some(waker) = inner.wakers.reads.remove(&connection_id) {
            waker.wake();
        }
    }

    /// Handle FIN delivery to a connection's receive side (TCP half-close).
    ///
    /// Sets `remote_fin_received` on the receiving connection so that `poll_read`
    /// returns EOF after the receive buffer is drained. Ignores FIN if the connection
    /// was already aborted (stale event).
    fn handle_fin_delivery(inner: &mut SimInner, connection_id: ConnectionId) {
        tracing::debug!(
            "FinDelivery: FIN received on connection {}",
            connection_id.0
        );

        // If connection was aborted after FIN was scheduled, ignore the stale FIN
        let is_closed = inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.flags.is_closed());

        if is_closed {
            tracing::debug!(
                "FinDelivery: connection {} already closed, ignoring stale FIN",
                connection_id.0
            );
            return;
        }

        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            conn.flags.set_remote_fin_received(true);
        }

        if let Some(waker) = inner.wakers.reads.remove(&connection_id) {
            waker.wake();
        }
    }

    /// Schedule a `FinDelivery` event to the peer connection.
    ///
    /// The FIN is scheduled after the last `DataDelivery` event to ensure all data
    /// arrives at the peer before EOF is signaled.
    fn schedule_fin_delivery(
        inner: &mut SimInner,
        paired_id: Option<ConnectionId>,
        last_delivery_time: Option<Duration>,
    ) {
        let Some(peer_id) = paired_id else {
            return;
        };

        // FIN must arrive after all DataDelivery events
        let fin_time = match last_delivery_time {
            Some(t) if t >= inner.current_time => t + Duration::from_nanos(1),
            _ => inner.current_time + Duration::from_nanos(1),
        };

        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        tracing::debug!(
            "Scheduling FinDelivery to connection {} at {:?}",
            peer_id.0,
            fin_time
        );

        inner.event_queue.schedule(ScheduledEvent::new(
            fin_time,
            Event::Network {
                connection_id: peer_id.0,
                operation: NetworkOperation::FinDelivery,
            },
            sequence,
        ));
    }

    /// Maybe corrupt data with bit flips based on chaos configuration.
    fn maybe_corrupt_data(
        inner: &mut SimInner,
        connection_id: ConnectionId,
        data: &[u8],
    ) -> Vec<u8> {
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
            // `% len` keeps the result well within `usize`; truncation is intentional.
            let raw_byte = sim_random::<u64>();
            let raw_bit = sim_random::<u64>();
            let len_u64 =
                u64::try_from(corrupted_data.len()).expect("corrupted_data length fits in u64");
            let byte_idx =
                usize::try_from(raw_byte % len_u64).expect("modulus bounded by usize length");
            let bit_idx = usize::try_from(raw_bit % 8).expect("modulus 8 fits in usize");
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

        inner.record_fault(SimFaultEvent::BitFlip {
            connection_id: connection_id.0,
            flip_count: flipped_positions.len(),
        });

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
            Self::wake_all(&mut inner.wakers.send_buffers, connection_id);

            if conn.send_buffer.is_empty() {
                conn.flags.set_send_in_progress(false);
                // Check for pending graceful close when pipeline drains
                if conn.flags.graceful_close_pending() {
                    conn.flags.set_graceful_close_pending(false);
                    let peer_id = conn.paired_connection;
                    let last_time = conn.last_data_delivery_scheduled_at;
                    Self::schedule_fin_delivery(inner, peer_id, last_time);
                }
            } else {
                Self::schedule_process_send_buffer(inner, connection_id);
            }
        } else {
            conn.flags.set_send_in_progress(false);
            // Check for pending graceful close when pipeline drains
            if conn.flags.graceful_close_pending() {
                conn.flags.set_graceful_close_pending(false);
                let peer_id = conn.paired_connection;
                let last_time = conn.last_data_delivery_scheduled_at;
                Self::schedule_fin_delivery(inner, peer_id, last_time);
            }
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
        let is_stable = conn.flags.is_stable(); // For stable connection checks
        let local_ip = conn.local_ip;
        let remote_ip = conn.remote_ip;

        let recv_delay = paired_id.and_then(|pid| {
            inner
                .network
                .connections
                .get(&pid)
                .and_then(|c| c.recv_delay)
        });

        // Permanent per-pair latency, memoized at connect (FDB SimClogging). Pure
        // map read on a field disjoint from `connections`; ZERO when the feature is
        // off (map empty) or endpoints are unknown.
        let pair_extra = match (local_ip, remote_ip) {
            (Some(src), Some(dst)) => inner
                .network
                .pair_latencies
                .get(&(src, dst))
                .copied()
                .unwrap_or(Duration::ZERO),
            _ => Duration::ZERO,
        };

        if !has_data {
            if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
                conn.flags.set_send_in_progress(false);
                // Check for pending graceful close when pipeline drains
                if conn.flags.graceful_close_pending() {
                    conn.flags.set_graceful_close_pending(false);
                    let peer_id = conn.paired_connection;
                    let last_time = conn.last_data_delivery_scheduled_at;
                    Self::schedule_fin_delivery(inner, peer_id, last_time);
                }
            }
            return;
        }

        let Some(conn) = inner.network.connections.get_mut(&connection_id) else {
            return;
        };

        let Some(mut data) = conn.send_buffer.pop_front() else {
            conn.flags.set_send_in_progress(false);
            return;
        };

        Self::wake_all(&mut inner.wakers.send_buffers, connection_id);

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
                crate::network::sample_latency(&inner.network.config.write_latency)
            })
        };

        let earliest_time = std::cmp::max(inner.current_time + base_delay, next_send_time);
        conn.next_send_time = earliest_time + Duration::from_nanos(1);

        // Schedule data delivery to paired connection
        if let Some(paired_id) = paired_id {
            let scheduled_time = earliest_time + recv_delay.unwrap_or(Duration::ZERO) + pair_extra;
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

            // Track delivery time for FIN ordering (must be after last DataDelivery)
            conn.last_data_delivery_scheduled_at = Some(scheduled_time);
        }

        // Schedule next send if more data
        if conn.send_buffer.is_empty() {
            conn.flags.set_send_in_progress(false);
            // Check for pending graceful close when pipeline drains
            if conn.flags.graceful_close_pending() {
                conn.flags.set_graceful_close_pending(false);
                let peer_id = conn.paired_connection;
                let last_time = conn.last_data_delivery_scheduled_at;
                Self::schedule_fin_delivery(inner, peer_id, last_time);
            }
        } else {
            Self::schedule_process_send_buffer(inner, connection_id);
        }
    }

    /// Schedule a `ProcessSendBuffer` event for the given connection.
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

    /// Handle shutdown event - wake all pending tasks.
    fn handle_shutdown_event(inner: &mut SimInner) {
        tracing::debug!("Processing Shutdown event - waking all pending tasks");

        for (task_id, waker) in std::mem::take(&mut inner.wakers.tasks) {
            tracing::trace!("Waking task {}", task_id);
            waker.wake();
        }

        for (_conn_id, waker) in std::mem::take(&mut inner.wakers.reads) {
            waker.wake();
        }

        tracing::debug!("Shutdown event processed");
    }

    /// Get current assertion results for all tracked assertions.
    #[must_use]
    pub fn assertion_results(
        &self,
    ) -> std::collections::HashMap<String, crate::chaos::AssertionStats> {
        crate::chaos::assertion_results()
    }

    /// Reset assertion statistics to empty state.
    pub fn reset_assertion_results(&self) {
        crate::chaos::reset_assertion_results();
    }

    /// Abort all connections involving a specific IP address.
    ///
    /// This is used during process reboot to immediately kill all network
    /// connections for the rebooted process. Both local and remote connections
    /// are aborted (RST semantics — peer sees ECONNRESET).
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn abort_all_connections_for_ip(&self, ip: std::net::IpAddr) {
        let connection_ids: Vec<ConnectionId> = {
            let inner = self
                .inner
                .read()
                .expect("RwLock poisoned: prior task panicked");
            inner
                .network
                .connections
                .iter()
                .filter_map(|(id, conn)| {
                    if conn.local_ip == Some(ip) || conn.remote_ip == Some(ip) {
                        Some(*id)
                    } else {
                        None
                    }
                })
                .collect()
        };

        let count = connection_ids.len();
        for conn_id in connection_ids {
            self.close_connection_abort(conn_id);
        }

        if count > 0 {
            tracing::debug!("Aborted {} connections for rebooted IP {}", count, ip);
        }
    }

    /// Schedule a `ProcessRestart` event after a recovery delay.
    ///
    /// Called after a process is killed to schedule its restart.
    pub fn schedule_process_restart(
        &self,
        ip: std::net::IpAddr,
        recovery_delay: std::time::Duration,
    ) {
        self.schedule_event(Event::ProcessRestart { ip }, recovery_delay);
        tracing::debug!(
            "Scheduled process restart for IP {} in {:?}",
            ip,
            recovery_delay
        );
    }

    /// Returns the last event processed by `step()`, if any.
    ///
    /// This is used by the orchestrator to detect `ProcessRestart` events
    /// and handle them (respawn the process).
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn last_processed_event(&self) -> Option<Event> {
        self.inner
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .last_processed_event
            .clone()
    }

    /// Extract simulation metrics (simulated time, events processed).
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn extract_metrics(&self) -> crate::runner::SimulationMetrics {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");

        crate::runner::SimulationMetrics {
            wall_time: std::time::Duration::ZERO,
            simulated_time: inner.current_time,
            events_processed: inner.events_processed,
        }
    }

    // Phase 7: Simple write clogging methods

    /// Check if a write should be clogged based on probability
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn should_clog_write(&self, connection_id: ConnectionId) -> bool {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        let config = &inner.network.config;

        // Skip stable connections (FDB: stableConnection exempt from chaos)
        if inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.flags.is_stable())
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
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn clog_write(&self, connection_id: ConnectionId) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
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
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn is_write_clogged(&self, connection_id: ConnectionId) -> bool {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");

        if let Some(clog_state) = inner.network.connection_clogs.get(&connection_id) {
            inner.current_time < clog_state.expires_at
        } else {
            false
        }
    }

    /// Register a waker for when write clog clears
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn register_clog_waker(&self, connection_id: ConnectionId, waker: Waker) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .wakers
            .write_clogs
            .entry(connection_id)
            .or_default()
            .push(waker);
    }

    // Read clogging methods (symmetric with write clogging)

    /// Check if a read should be clogged based on probability
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn should_clog_read(&self, connection_id: ConnectionId) -> bool {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        let config = &inner.network.config;

        // Skip stable connections (FDB: stableConnection exempt from chaos)
        if inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.flags.is_stable())
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
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn clog_read(&self, connection_id: ConnectionId) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
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
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn is_read_clogged(&self, connection_id: ConnectionId) -> bool {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");

        if let Some(clog_state) = inner.network.read_clogs.get(&connection_id) {
            inner.current_time < clog_state.expires_at
        } else {
            false
        }
    }

    /// Register a waker for when read clog clears
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn register_read_clog_waker(&self, connection_id: ConnectionId, waker: Waker) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .wakers
            .read_clogs
            .entry(connection_id)
            .or_default()
            .push(waker);
    }

    /// Clear expired clogs and wake pending tasks
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn clear_expired_clogs(&self) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        let now = inner.current_time;
        let expired: Vec<ConnectionId> = inner
            .network
            .connection_clogs
            .iter()
            .filter_map(|(id, state)| (now >= state.expires_at).then_some(*id))
            .collect();

        for id in expired {
            inner.network.connection_clogs.remove(&id);
            Self::wake_all(&mut inner.wakers.write_clogs, id);
        }
    }

    // Connection Cut Methods (temporary network outage simulation)

    /// Check if a connection is temporarily cut.
    ///
    /// A cut connection is temporarily unavailable but will be restored.
    /// This is different from `is_connection_closed` which indicates permanent closure.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn is_connection_cut(&self, connection_id: ConnectionId) -> bool {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| {
                conn.flags.is_cut()
                    && conn
                        .cut_expiry
                        .is_some_and(|expiry| inner.current_time < expiry)
            })
    }

    /// Register a waker for when a cut connection is restored.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn register_cut_waker(&self, connection_id: ConnectionId, waker: Waker) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .wakers
            .cuts
            .entry(connection_id)
            .or_default()
            .push(waker);
    }

    // Send buffer management methods

    /// Get the send buffer capacity for a connection.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn send_buffer_capacity(&self, connection_id: ConnectionId) -> usize {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .network
            .connections
            .get(&connection_id)
            .map_or(0, |conn| conn.send_buffer_capacity)
    }

    /// Get the current send buffer usage for a connection.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn send_buffer_used(&self, connection_id: ConnectionId) -> usize {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .network
            .connections
            .get(&connection_id)
            .map_or(0, |conn| {
                conn.send_buffer.iter().map(std::vec::Vec::len).sum()
            })
    }

    /// Get the available send buffer space for a connection.
    #[must_use]
    pub fn available_send_buffer(&self, connection_id: ConnectionId) -> usize {
        let capacity = self.send_buffer_capacity(connection_id);
        let used = self.send_buffer_used(connection_id);
        capacity.saturating_sub(used)
    }

    /// Register a waker for when send buffer space becomes available.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn register_send_buffer_waker(&self, connection_id: ConnectionId, waker: Waker) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .wakers
            .send_buffers
            .entry(connection_id)
            .or_default()
            .push(waker);
    }

    // Per-IP-pair base latency methods

    /// Get the base latency for a connection pair.
    /// Returns the latency if already set, otherwise None.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn pair_latency(&self, src: IpAddr, dst: IpAddr) -> Option<Duration> {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        inner.network.pair_latencies.get(&(src, dst)).copied()
    }

    /// Set the base latency for a connection pair if not already set.
    /// Returns the latency (existing or newly set).
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn set_pair_latency_if_not_set(
        &self,
        src: IpAddr,
        dst: IpAddr,
        latency: Duration,
    ) -> Duration {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
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

    /// Get the permanent per-pair base latency for a connection, memoizing it on
    /// first contact (FDB `SimClogging`).
    ///
    /// Returns [`Duration::ZERO`] — without drawing from the RNG or touching the
    /// pair map — when the `max_pair_latency` range is disabled (its `end` is zero)
    /// or the connection's endpoints are unknown. Otherwise samples a fixed latency
    /// from `max_pair_latency` once per ordered IP pair and reuses it for the run.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn connection_base_latency(&self, connection_id: ConnectionId) -> Duration {
        let range = self.with_network_config(|config| config.chaos.max_pair_latency.clone());
        if range.end.is_zero() {
            return Duration::ZERO;
        }

        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        let endpoints = inner
            .network
            .connections
            .get(&connection_id)
            .and_then(|conn| Some((conn.local_ip?, conn.remote_ip?)));
        drop(inner);

        let Some((local_ip, remote_ip)) = endpoints else {
            return Duration::ZERO;
        };

        // Check if latency is already set
        if let Some(latency) = self.pair_latency(local_ip, remote_ip) {
            return latency;
        }

        // Sample a fixed latency for this pair and memoize it for the whole run.
        let latency = crate::network::sample_duration(&range);
        self.set_pair_latency_if_not_set(local_ip, remote_ip, latency)
    }

    // Per-connection asymmetric delay methods

    /// Get the send delay for a connection.
    /// Returns the per-connection override if set, otherwise None.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn send_delay(&self, connection_id: ConnectionId) -> Option<Duration> {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .network
            .connections
            .get(&connection_id)
            .and_then(|conn| conn.send_delay)
    }

    /// Get the receive delay for a connection.
    /// Returns the per-connection override if set, otherwise None.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn recv_delay(&self, connection_id: ConnectionId) -> Option<Duration> {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .network
            .connections
            .get(&connection_id)
            .and_then(|conn| conn.recv_delay)
    }

    /// Check if a connection is permanently closed
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn is_connection_closed(&self, connection_id: ConnectionId) -> bool {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.flags.is_closed())
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
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn close_reason(&self, connection_id: ConnectionId) -> CloseReason {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .network
            .connections
            .get(&connection_id)
            .map_or(CloseReason::None, |conn| conn.close_reason)
    }

    /// Close a connection with a specific close reason.
    fn close_connection_with_reason(&self, connection_id: ConnectionId, reason: CloseReason) {
        match reason {
            CloseReason::Graceful => self.close_connection_graceful(connection_id),
            CloseReason::Aborted => self.close_connection_aborted(connection_id),
            CloseReason::None => {}
        }
    }

    /// Graceful close (FIN semantics) — TCP half-close.
    ///
    /// Marks the local write side as closed. The peer can still read all buffered
    /// and in-flight data. A `FinDelivery` event is scheduled after the last
    /// `DataDelivery` to signal EOF to the peer.
    fn close_connection_graceful(&self, connection_id: ConnectionId) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");

        // Extract connection info
        let conn_info = inner.network.connections.get(&connection_id).map(|conn| {
            (
                conn.paired_connection,
                conn.flags.send_closed(),
                conn.flags.is_closed(),
                conn.flags.send_in_progress(),
                conn.send_buffer.is_empty(),
                conn.last_data_delivery_scheduled_at,
            )
        });

        let Some((
            paired_id,
            was_send_closed,
            was_closed,
            send_in_progress,
            send_buffer_empty,
            last_delivery_time,
        )) = conn_info
        else {
            return;
        };

        // Idempotent: if already closed or send_closed (by chaos or previous close), skip
        if was_closed || was_send_closed {
            tracing::debug!(
                "Connection {} already closed/send_closed, skipping graceful close",
                connection_id.0
            );
            return;
        }

        // Mark local side as closed with send side shut down
        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            conn.flags.set_is_closed(true);
            conn.close_reason = CloseReason::Graceful;
            conn.flags.set_send_closed(true);
            tracing::debug!(
                "Connection {} graceful close (FIN) - local write shut down",
                connection_id.0
            );
        }

        // Wake local read waker (so local poll_read returns EOF if needed)
        if let Some(waker) = inner.wakers.reads.remove(&connection_id) {
            waker.wake();
        }

        // Do NOT set is_closed on the peer — they can still read buffered data.
        // Instead, schedule a FIN delivery after all data has been delivered.
        if send_in_progress || !send_buffer_empty {
            // Pipeline has data — defer FIN until pipeline drains
            if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
                conn.flags.set_graceful_close_pending(true);
                tracing::debug!(
                    "Connection {} graceful close deferred (send pipeline active)",
                    connection_id.0
                );
            }
        } else {
            // Pipeline is idle — schedule FIN delivery now
            Self::schedule_fin_delivery(&mut inner, paired_id, last_delivery_time);
        }
    }

    /// Aborted close (RST semantics) — immediate connection kill.
    ///
    /// Immediately sets `is_closed` on both endpoints. The peer will receive
    /// ECONNRESET on both read and write operations.
    fn close_connection_aborted(&self, connection_id: ConnectionId) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");

        let paired_connection_id = inner
            .network
            .connections
            .get(&connection_id)
            .and_then(|conn| conn.paired_connection);

        if let Some(conn) = inner.network.connections.get_mut(&connection_id)
            && !conn.flags.is_closed()
        {
            conn.flags.set_is_closed(true);
            conn.close_reason = CloseReason::Aborted;
            // Cancel any pending graceful close
            conn.flags.set_graceful_close_pending(false);
            tracing::debug!(
                "Connection {} closed permanently (reason: Aborted)",
                connection_id.0
            );
        }

        if let Some(paired_id) = paired_connection_id
            && let Some(paired_conn) = inner.network.connections.get_mut(&paired_id)
            && !paired_conn.flags.is_closed()
        {
            paired_conn.flags.set_is_closed(true);
            paired_conn.close_reason = CloseReason::Aborted;
            tracing::debug!(
                "Paired connection {} also closed (reason: Aborted)",
                paired_id.0
            );
        }

        if let Some(waker) = inner.wakers.reads.remove(&connection_id) {
            tracing::debug!(
                "Waking read waker for aborted connection {}",
                connection_id.0
            );
            waker.wake();
        }

        if let Some(paired_id) = paired_connection_id
            && let Some(paired_waker) = inner.wakers.reads.remove(&paired_id)
        {
            tracing::debug!(
                "Waking read waker for paired aborted connection {}",
                paired_id.0
            );
            paired_waker.wake();
        }
    }

    /// Close connection asymmetrically (FDB rollRandomClose pattern)
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn close_connection_asymmetric(
        &self,
        connection_id: ConnectionId,
        close_send: bool,
        close_recv: bool,
    ) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");

        let paired_id = inner
            .network
            .connections
            .get(&connection_id)
            .and_then(|conn| conn.paired_connection);

        if close_send && let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            conn.flags.set_send_closed(true);
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
            paired_conn.flags.set_recv_closed(true);
            tracing::debug!(
                "Connection {} recv side closed (asymmetric via peer)",
                paired.0
            );
        }

        if close_send && let Some(waker) = inner.wakers.reads.remove(&connection_id) {
            waker.wake();
        }
        if close_recv
            && let Some(paired) = paired_id
            && let Some(waker) = inner.wakers.reads.remove(&paired)
        {
            waker.wake();
        }
    }

    /// Roll random close chaos injection (FDB rollRandomClose pattern)
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn roll_random_close(&self, connection_id: ConnectionId) -> Option<bool> {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        let config = &inner.network.config;

        // Skip stable connections (FDB: stableConnection exempt from chaos)
        if inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.flags.is_stable())
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

        inner.record_fault(SimFaultEvent::RandomClose {
            connection_id: connection_id.0,
        });

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
            conn.flags.set_send_closed(true);
            conn.send_buffer.clear();
        }

        if close_recv
            && let Some(paired) = paired_id
            && let Some(paired_conn) = inner.network.connections.get_mut(&paired)
        {
            paired_conn.flags.set_recv_closed(true);
        }

        if close_send && let Some(waker) = inner.wakers.reads.remove(&connection_id) {
            waker.wake();
        }
        if close_recv
            && let Some(paired) = paired_id
            && let Some(waker) = inner.wakers.reads.remove(&paired)
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
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn is_send_closed(&self, connection_id: ConnectionId) -> bool {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.flags.send_closed() || conn.flags.is_closed())
    }

    /// Check if a connection's receive side is closed
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn is_recv_closed(&self, connection_id: ConnectionId) -> bool {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.flags.recv_closed() || conn.flags.is_closed())
    }

    /// Check if a FIN has been received from the remote peer (graceful close).
    ///
    /// When true, `poll_read` should return EOF after draining the receive buffer.
    /// Distinct from `is_recv_closed` which is used for chaos/asymmetric closure.
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn is_remote_fin_received(&self, connection_id: ConnectionId) -> bool {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.flags.remote_fin_received())
    }

    // Half-Open Connection Simulation Methods

    /// Check if a connection is in half-open state
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn is_half_open(&self, connection_id: ConnectionId) -> bool {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.flags.is_half_open())
    }

    /// Check if a half-open connection should return errors now
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    #[must_use]
    pub fn should_half_open_error(&self, connection_id: ConnectionId) -> bool {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        let current_time = inner.current_time;
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| {
                conn.flags.is_half_open()
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
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    pub fn mark_connection_stable(&self, connection_id: ConnectionId) {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            conn.flags.set_is_stable(true);
            tracing::debug!("Connection {} marked as stable", connection_id.0);

            // Also mark the paired connection as stable
            if let Some(paired_id) = conn.paired_connection
                && let Some(paired_conn) = inner.network.connections.get_mut(&paired_id)
            {
                paired_conn.flags.set_is_stable(true);
                tracing::debug!("Paired connection {} also marked as stable", paired_id.0);
            }
        }
    }

    // Network Partition Control Methods

    /// Partition communication between two IP addresses for a specified duration
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation is rejected by the simulator (for example, the simulation has not been started).
    pub fn partition_pair(
        &self,
        from_ip: std::net::IpAddr,
        to_ip: std::net::IpAddr,
        duration: Duration,
    ) -> SimulationResult<()> {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
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

        inner.record_fault(SimFaultEvent::PartitionCreated {
            from: from_ip.to_string(),
            to: to_ip.to_string(),
        });

        tracing::debug!(
            "Partitioned {} -> {} until {:?}",
            from_ip,
            to_ip,
            expires_at
        );
        Ok(())
    }

    /// Block all outgoing communication from an IP address
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation is rejected by the simulator (for example, the simulation has not been started).
    pub fn partition_send_from(
        &self,
        ip: std::net::IpAddr,
        duration: Duration,
    ) -> SimulationResult<()> {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
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

        inner.record_fault(SimFaultEvent::SendPartitionCreated { ip: ip.to_string() });
        tracing::debug!("Partitioned sends from {} until {:?}", ip, expires_at);
        Ok(())
    }

    /// Block all incoming communication to an IP address
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation is rejected by the simulator (for example, the simulation has not been started).
    pub fn partition_recv_to(
        &self,
        ip: std::net::IpAddr,
        duration: Duration,
    ) -> SimulationResult<()> {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
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

        inner.record_fault(SimFaultEvent::RecvPartitionCreated { ip: ip.to_string() });
        tracing::debug!("Partitioned receives to {} until {:?}", ip, expires_at);
        Ok(())
    }

    /// Immediately restore communication between two IP addresses
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation is rejected by the simulator (for example, the simulation has not been started).
    pub fn restore_partition(
        &self,
        from_ip: std::net::IpAddr,
        to_ip: std::net::IpAddr,
    ) -> SimulationResult<()> {
        let mut inner = self
            .inner
            .write()
            .expect("RwLock poisoned: prior task panicked");
        inner.network.ip_partitions.remove(&(from_ip, to_ip));
        inner.record_fault(SimFaultEvent::PartitionHealed {
            from: from_ip.to_string(),
            to: to_ip.to_string(),
        });
        tracing::debug!("Restored partition {} -> {}", from_ip, to_ip);
        Ok(())
    }

    /// Check if communication between two IP addresses is currently partitioned
    ///
    /// # Panics
    ///
    /// Panics if the simulation lock is poisoned by a prior task panic.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation is rejected by the simulator (for example, the simulation has not been started).
    pub fn is_partitioned(
        &self,
        from_ip: std::net::IpAddr,
        to_ip: std::net::IpAddr,
    ) -> SimulationResult<bool> {
        let inner = self
            .inner
            .read()
            .expect("RwLock poisoned: prior task panicked");
        Ok(inner
            .network
            .is_partitioned(from_ip, to_ip, inner.current_time))
    }

    /// Helper method for use with `SimInner` - randomly trigger partitions
    ///
    /// Supports different partition strategies based on configuration:
    /// - Random: randomly partition individual IP pairs
    /// - `UniformSize`: create uniform-sized partition groups
    /// - `IsolateSingle`: isolate exactly one node from the rest
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

                // Disjoint-field push: `partition_config` still borrows
                // `inner.network`, so go through `pending_faults` directly.
                let time_ms = u64::try_from(inner.current_time.as_millis()).unwrap_or(u64::MAX);
                inner.pending_faults.push(SimFaultRecord {
                    time_ms,
                    event: SimFaultEvent::PartitionCreated {
                        from: from_ip.to_string(),
                        to: to_ip.to_string(),
                    },
                });

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
    pub(crate) inner: Weak<RwLock<SimInner>>,
}

/// Macro to generate `WeakSimWorld` forwarding methods that wrap `SimWorld` results.
macro_rules! weak_forward {
    // For methods returning T that need Ok() wrapping
    (wrap $(#[$meta:meta])* $method:ident(&self $(, $arg:ident : $arg_ty:ty)*) -> $ret:ty) => {
        $(#[$meta])*
        ///
        /// # Errors
        ///
        /// Returns an error if the simulation has been dropped.
        pub fn $method(&self $(, $arg: $arg_ty)*) -> SimulationResult<$ret> {
            Ok(self.upgrade()?.$method($($arg),*))
        }
    };
    // For methods already returning SimulationResult
    (pass $(#[$meta:meta])* $method:ident(&self $(, $arg:ident : $arg_ty:ty)*) -> $ret:ty) => {
        $(#[$meta])*
        ///
        /// # Errors
        ///
        /// Returns an error if the simulation has been dropped or the
        /// underlying operation is rejected by the simulator.
        pub fn $method(&self $(, $arg: $arg_ty)*) -> SimulationResult<$ret> {
            self.upgrade()?.$method($($arg),*)
        }
    };
    // For methods returning () that need Ok(()) wrapping
    (unit $(#[$meta:meta])* $method:ident(&self $(, $arg:ident : $arg_ty:ty)*)) => {
        $(#[$meta])*
        ///
        /// # Errors
        ///
        /// Returns an error if the simulation has been dropped.
        pub fn $method(&self $(, $arg: $arg_ty)*) -> SimulationResult<()> {
            self.upgrade()?.$method($($arg),*);
            Ok(())
        }
    };
}

impl WeakSimWorld {
    /// Attempts to upgrade this weak reference to a strong reference.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation is rejected by the simulator (for example, the simulation has not been started).
    pub fn upgrade(&self) -> SimulationResult<SimWorld> {
        self.inner
            .upgrade()
            .map(|inner| SimWorld { inner })
            .ok_or(SimulationError::SimulationShutdown)
    }

    weak_forward!(wrap #[doc = "Returns the current simulation time."] current_time(&self) -> Duration);
    weak_forward!(wrap #[doc = "Returns the exact simulation time (equivalent to FDB's `now()`)."] now(&self) -> Duration);
    weak_forward!(wrap #[doc = "Returns the drifted timer time (equivalent to FDB's `timer()`)."] timer(&self) -> Duration);
    weak_forward!(unit #[doc = "Schedules an event to execute after the specified delay."] schedule_event(&self, event: Event, delay: Duration));
    weak_forward!(unit #[doc = "Schedules an event to execute at the specified absolute time."] schedule_event_at(&self, event: Event, time: Duration));
    weak_forward!(pass #[doc = "Read data from connection's receive buffer."] read_from_connection(&self, connection_id: ConnectionId, buf: &mut [u8]) -> usize);
    weak_forward!(pass #[doc = "Write data to connection's receive buffer."] write_to_connection(&self, connection_id: ConnectionId, data: &[u8]) -> ());
    weak_forward!(pass #[doc = "Buffer data for ordered sending on a connection."] buffer_send(&self, connection_id: ConnectionId, data: Vec<u8>) -> ());
    weak_forward!(wrap #[doc = "Get a network provider for the simulation."] network_provider(&self) -> SimNetworkProvider);
    weak_forward!(wrap #[doc = "Get a time provider for the simulation."] time_provider(&self) -> crate::providers::SimTimeProvider);
    weak_forward!(wrap #[doc = "Sleep for the specified duration in simulation time."] sleep(&self, duration: Duration) -> SleepFuture);

    /// Access network configuration for latency calculations.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation is rejected by the simulator (for example, the simulation has not been started).
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
    fn pair_latency_deterministic_per_seed() {
        use crate::network::{ChaosConfiguration, NetworkConfiguration};

        let client_ip = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 1, 1));
        let server_ip = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 1, 2));
        let range = Duration::from_millis(10)..Duration::from_millis(50);

        // Build a fast-local config that only enables per-pair latency.
        let on_config = || NetworkConfiguration {
            chaos: ChaosConfiguration {
                max_pair_latency: range.clone(),
                ..ChaosConfiguration::disabled()
            },
            ..NetworkConfiguration::fast_local()
        };

        // Establish one connection pair and memoize both directions.
        let assign = || {
            let sim = SimWorld::new_with_network_config_and_seed(on_config(), 42);
            let (client, server) = sim.create_connection_pair("10.0.1.1:0", "10.0.1.2:0");
            let _ = sim.connection_base_latency(client);
            let _ = sim.connection_base_latency(server);
            (
                sim.pair_latency(client_ip, server_ip),
                sim.pair_latency(server_ip, client_ip),
            )
        };

        let first = assign();
        let second = assign();

        // Same seed -> identical, fully-populated assignments.
        assert_eq!(first, second, "pair latency must be deterministic per seed");
        let c2s = first.0.expect("client->server pair latency assigned");
        let s2c = first.1.expect("server->client pair latency assigned");
        for latency in [c2s, s2c] {
            assert!(
                range.contains(&latency),
                "pair latency {latency:?} must fall in the configured range {range:?}"
            );
        }

        // Disabled range (default) -> no map writes at all.
        let off =
            SimWorld::new_with_network_config_and_seed(NetworkConfiguration::fast_local(), 42);
        let (client, server) = off.create_connection_pair("10.0.1.1:0", "10.0.1.2:0");
        let _ = off.connection_base_latency(client);
        let _ = off.connection_base_latency(server);
        assert_eq!(off.pair_latency(client_ip, server_ip), None);
        assert_eq!(off.pair_latency(server_ip, client_ip), None);
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

    #[test]
    fn record_fault_stamps_current_time() {
        let mut inner = SimInner::new();
        inner.current_time = Duration::from_millis(500);
        inner.record_fault(SimFaultEvent::StorageCrash {
            ip: "10.0.1.1".to_string(),
        });

        assert_eq!(inner.pending_faults.len(), 1);
        let record = &inner.pending_faults[0];
        assert_eq!(record.time_ms, 500);
        assert!(matches!(record.event, SimFaultEvent::StorageCrash { .. }));
    }

    #[test]
    fn partition_pair_records_fault() {
        let sim = SimWorld::new();
        let from: std::net::IpAddr = "10.0.1.1".parse().expect("valid ip");
        let to: std::net::IpAddr = "10.0.1.2".parse().expect("valid ip");
        sim.partition_pair(from, to, Duration::from_secs(10))
            .expect("partition should succeed");

        let faults = sim.take_faults();
        assert_eq!(faults.len(), 1);
        assert!(matches!(
            &faults[0].event,
            SimFaultEvent::PartitionCreated { from, to }
            if from == "10.0.1.1" && to == "10.0.1.2"
        ));
        assert!(sim.take_faults().is_empty(), "drained on take");
    }

    #[test]
    fn restore_partition_records_fault() {
        let sim = SimWorld::new();
        let from: std::net::IpAddr = "10.0.1.1".parse().expect("valid ip");
        let to: std::net::IpAddr = "10.0.1.2".parse().expect("valid ip");
        sim.partition_pair(from, to, Duration::from_secs(10))
            .expect("partition");
        sim.restore_partition(from, to).expect("restore");

        let faults = sim.take_faults();
        assert_eq!(faults.len(), 2);
        assert!(matches!(
            &faults[0].event,
            SimFaultEvent::PartitionCreated { .. }
        ));
        assert!(matches!(
            &faults[1].event,
            SimFaultEvent::PartitionHealed { .. }
        ));
    }
}
