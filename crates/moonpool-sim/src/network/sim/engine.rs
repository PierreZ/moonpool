//! Stateful network simulation independent from the global event scheduler.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    net::IpAddr,
    task::Waker,
    time::Duration,
};

use crate::{
    SimulationError, SimulationResult, assert_reachable,
    chaos::fault_events::SimFaultEvent,
    locality::{DomainLevel, LocalityInfo},
    network::{LinkLatencyConfig, NetworkConfiguration, PartitionStrategy},
    sim::{
        rng::{sim_random, sim_random_f64, sim_random_range},
        wakers::{WakeBatch, WakerRegistry},
    },
};

use super::{
    ConnectionId, ListenerId, NetworkEvent,
    event::NetworkOperationId,
    state::{
        ClogState, CloseReason, ConnectionFlags, ConnectionState, NetworkState, PartitionState,
    },
};

#[derive(Debug, Default)]
struct NetworkWaiters {
    accepts: BTreeMap<String, BTreeMap<AcceptWaiterId, Waker>>,
    reads: WakerRegistry<ConnectionId>,
    write_clogs: WakerRegistry<ConnectionId>,
    read_clogs: WakerRegistry<ConnectionId>,
    send_buffers: WakerRegistry<ConnectionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct AcceptWaiterId(u64);

#[derive(Debug)]
struct AcceptReservation {
    addr: String,
    connection_id: ConnectionId,
    waker: Waker,
}

/// Ordered changes emitted by the network engine for the coordinator to apply.
#[derive(Debug, Default)]
pub(crate) struct NetworkActions {
    scheduled: Vec<(Duration, NetworkEvent)>,
    faults: Vec<SimFaultEvent>,
}

impl NetworkActions {
    pub(crate) fn schedule_at(&mut self, at: Duration, event: NetworkEvent) {
        self.scheduled.push((at, event));
    }

    pub(crate) fn record(&mut self, fault: SimFaultEvent) {
        self.faults.push(fault);
    }

    pub(crate) fn into_parts(self) -> (Vec<(Duration, NetworkEvent)>, Vec<SimFaultEvent>) {
        (self.scheduled, self.faults)
    }
}

/// Network protocol state, topology, fault injection, and resource waiters.
#[derive(Debug)]
pub(crate) struct NetworkSimulation {
    state: NetworkState,
    waiters: NetworkWaiters,
    localities: BTreeMap<IpAddr, LocalityInfo>,
    last_bit_flip_time: Duration,
    next_operation_id: u64,
    next_accept_waiter_id: u64,
    completed_operations: BTreeSet<NetworkOperationId>,
    failed_operations: BTreeSet<NetworkOperationId>,
    failed_accepts: BTreeSet<AcceptWaiterId>,
    accept_reservations: BTreeMap<AcceptWaiterId, AcceptReservation>,
    operation_waiters: WakerRegistry<NetworkOperationId>,
}

impl NetworkSimulation {
    pub(crate) fn new(config: NetworkConfiguration) -> Self {
        Self {
            state: NetworkState::new(config),
            waiters: NetworkWaiters::default(),
            localities: BTreeMap::new(),
            last_bit_flip_time: Duration::ZERO,
            next_operation_id: 0,
            next_accept_waiter_id: 0,
            completed_operations: BTreeSet::new(),
            failed_operations: BTreeSet::new(),
            failed_accepts: BTreeSet::new(),
            accept_reservations: BTreeMap::new(),
            operation_waiters: WakerRegistry::default(),
        }
    }

    pub(crate) fn config(&self) -> &NetworkConfiguration {
        &self.state.config
    }

    #[cfg(test)]
    pub(crate) fn connection_count(&self) -> usize {
        self.state.connections.len()
    }

    pub(crate) fn set_config(
        &mut self,
        config: NetworkConfiguration,
        now: Duration,
    ) -> NetworkActions {
        let schedule_maintenance = config.chaos.partition_probability > 0.0;
        self.state.config = config;
        let mut actions = NetworkActions::default();
        if schedule_maintenance {
            actions.schedule_at(now, NetworkEvent::Maintenance);
        }
        actions
    }

    pub(crate) fn set_localities(&mut self, localities: BTreeMap<IpAddr, LocalityInfo>) {
        self.localities = localities;
    }

    pub(crate) fn create_listener(&mut self) -> ListenerId {
        let id = ListenerId(self.state.next_listener_id);
        self.state.next_listener_id += 1;
        self.state.listeners.insert(id);
        id
    }

    pub(crate) fn read(
        &mut self,
        connection_id: ConnectionId,
        buf: &mut [u8],
    ) -> SimulationResult<usize> {
        let partial_read_max_bytes = self.state.config.chaos.partial_read_max_bytes;
        let connection = self
            .state
            .connections
            .get_mut(&connection_id)
            .ok_or_else(|| SimulationError::InvalidState("connection not found".to_string()))?;
        let available = buf.len().min(connection.receive_buffer.len());
        let limit = if available > 0 && crate::buggify!() {
            let max_read = available.min(partial_read_max_bytes);
            if max_read >= 1 {
                sim_random_range(1..max_read + 1)
            } else {
                available
            }
        } else {
            available
        };
        for (slot, byte) in buf.iter_mut().zip(connection.receive_buffer.drain(..limit)) {
            *slot = byte;
        }
        Ok(limit)
    }

    pub(crate) fn has_readable_data(&self, connection_id: ConnectionId) -> bool {
        self.state
            .connections
            .get(&connection_id)
            .is_some_and(|connection| !connection.receive_buffer.is_empty())
    }

    pub(crate) fn buffer_send(
        &mut self,
        connection_id: ConnectionId,
        data: Vec<u8>,
        now: Duration,
    ) -> SimulationResult<NetworkActions> {
        let connection = self
            .state
            .connections
            .get_mut(&connection_id)
            .ok_or_else(|| SimulationError::InvalidState("connection not found".to_string()))?;
        connection.send_buffer.push_back(data);
        let mut actions = NetworkActions::default();
        if !connection.flags.send_in_progress() {
            connection.flags.set_send_in_progress(true);
            actions.schedule_at(now, NetworkEvent::ProcessSendBuffer { connection_id });
        }
        Ok(actions)
    }

    pub(crate) fn create_connection_pair(
        &mut self,
        client_addr: &str,
        server_addr: &str,
        now: Duration,
    ) -> (ConnectionId, ConnectionId) {
        const DEFAULT_SEND_BUFFER_CAPACITY: usize = 64 * 1024;
        let client_conn = ConnectionId(self.state.next_connection_id);
        self.state.next_connection_id += 1;
        let server_conn = ConnectionId(self.state.next_connection_id);
        self.state.next_connection_id += 1;
        let client_endpoint = NetworkState::parse_ip_from_addr(client_addr);
        let server_endpoint = NetworkState::parse_ip_from_addr(server_addr);
        let ephemeral_peer_addr = match client_endpoint {
            Some(IpAddr::V4(ipv4)) => {
                let octets = ipv4.octets();
                let offset =
                    u8::try_from(sim_random_range(0u32..256)).expect("range bounded to u8");
                let ip = std::net::Ipv4Addr::new(
                    octets[0],
                    octets[1],
                    octets[2],
                    octets[3].wrapping_add(offset),
                );
                let port = sim_random_range(40000u16..60000);
                format!("{ip}:{port}")
            }
            Some(IpAddr::V6(ipv6)) => {
                let mut segments = ipv6.segments();
                segments[7] = segments[7].wrapping_add(sim_random_range(0u16..256));
                let ip = std::net::Ipv6Addr::from(segments);
                let port = sim_random_range(40000u16..60000);
                format!("[{ip}]:{port}")
            }
            None => format!("unknown:{}", sim_random_range(40000u16..60000)),
        };
        let connection = |local_ip, remote_ip, peer_address, paired_connection| ConnectionState {
            local_ip,
            remote_ip,
            peer_address,
            receive_buffer: VecDeque::new(),
            paired_connection: Some(paired_connection),
            send_buffer: VecDeque::new(),
            next_send_time: now,
            flags: ConnectionFlags::default(),
            close_reason: CloseReason::None,
            send_buffer_capacity: DEFAULT_SEND_BUFFER_CAPACITY,
            last_data_delivery_scheduled_at: None,
        };
        self.state.connections.insert(
            client_conn,
            connection(
                client_endpoint,
                server_endpoint,
                server_addr.to_owned(),
                server_conn,
            ),
        );
        self.state.connections.insert(
            server_conn,
            connection(
                server_endpoint,
                client_endpoint,
                ephemeral_peer_addr,
                client_conn,
            ),
        );
        (client_conn, server_conn)
    }

    pub(crate) fn discard_connection_pair(&mut self, id: ConnectionId) -> WakeBatch {
        let paired = self
            .state
            .connections
            .get(&id)
            .and_then(|connection| connection.paired_connection);
        let mut wakes = WakeBatch::default();
        for current in [Some(id), paired].into_iter().flatten() {
            self.state.connections.remove(&current);
            self.state.connection_clogs.remove(&current);
            self.state.read_clogs.remove(&current);
            wakes.push(self.waiters.reads.take(&current));
            Self::take_waiter(&mut self.waiters.write_clogs, current, &mut wakes);
            Self::take_waiter(&mut self.waiters.read_clogs, current, &mut wakes);
            Self::take_waiter(&mut self.waiters.send_buffers, current, &mut wakes);
            for queue in self.state.pending_connections.values_mut() {
                queue.retain(|connection| *connection != current);
            }
        }
        self.state
            .pending_connections
            .retain(|_, queue| !queue.is_empty());
        wakes
    }

    pub(crate) fn register_read(&mut self, id: ConnectionId, waker: &Waker) -> bool {
        if self
            .state
            .connections
            .get(&id)
            .is_none_or(|connection| connection.flags.is_closed())
        {
            return false;
        }
        self.waiters.reads.register(id, waker);
        true
    }

    pub(crate) fn allocate_accept_waiter(&mut self) -> Option<AcceptWaiterId> {
        let id = AcceptWaiterId(self.next_accept_waiter_id);
        self.next_accept_waiter_id = self.next_accept_waiter_id.checked_add(1)?;
        Some(id)
    }

    pub(crate) fn poll_accept(
        &mut self,
        addr: &str,
        id: AcceptWaiterId,
        waker: Waker,
    ) -> SimulationResult<Option<ConnectionId>> {
        if self.failed_accepts.remove(&id) {
            return Err(SimulationError::SimulationShutdown);
        }
        if let Some(reservation) = self.accept_reservations.remove(&id) {
            return Ok(Some(reservation.connection_id));
        }
        if let Some(connection_id) = self.pending_connection(addr) {
            return Ok(Some(connection_id));
        }
        let waiters = self.waiters.accepts.entry(addr.to_string()).or_default();
        if !waiters
            .get(&id)
            .is_some_and(|registered| registered.will_wake(&waker))
        {
            waiters.insert(id, waker);
        }
        Ok(None)
    }

    pub(crate) fn cancel_accept(&mut self, addr: &str, id: AcceptWaiterId) -> WakeBatch {
        self.failed_accepts.remove(&id);
        if let Some(waiters) = self.waiters.accepts.get_mut(addr) {
            waiters.remove(&id);
            if waiters.is_empty() {
                self.waiters.accepts.remove(addr);
            }
        }
        if let Some(reservation) = self.accept_reservations.remove(&id) {
            return self.return_pending_connection(&reservation.addr, reservation.connection_id);
        }
        WakeBatch::default()
    }

    pub(crate) fn store_pending(&mut self, addr: &str, connection_id: ConnectionId) -> WakeBatch {
        if let Some((waiter_id, waker)) = self.take_next_accept(addr) {
            self.accept_reservations.insert(
                waiter_id,
                AcceptReservation {
                    addr: addr.to_string(),
                    connection_id,
                    waker: waker.clone(),
                },
            );
            let mut wakes = WakeBatch::default();
            wakes.extend([waker]);
            return wakes;
        }
        self.state
            .pending_connections
            .entry(addr.to_string())
            .or_default()
            .push_back(connection_id);
        WakeBatch::default()
    }

    pub(crate) fn pending_connection(&mut self, addr: &str) -> Option<ConnectionId> {
        let queue = self.state.pending_connections.get_mut(addr)?;
        let result = queue.pop_front();
        let empty = queue.is_empty();
        if empty {
            self.state.pending_connections.remove(addr);
        }
        result
    }

    pub(crate) fn return_pending_connection(
        &mut self,
        addr: &str,
        connection_id: ConnectionId,
    ) -> WakeBatch {
        if let Some((waiter_id, waker)) = self.take_next_accept(addr) {
            self.accept_reservations.insert(
                waiter_id,
                AcceptReservation {
                    addr: addr.to_string(),
                    connection_id,
                    waker: waker.clone(),
                },
            );
            let mut wakes = WakeBatch::default();
            wakes.extend([waker]);
            return wakes;
        }
        self.state
            .pending_connections
            .entry(addr.to_string())
            .or_default()
            .push_front(connection_id);
        WakeBatch::default()
    }

    fn take_next_accept(&mut self, addr: &str) -> Option<(AcceptWaiterId, Waker)> {
        let waiters = self.waiters.accepts.get_mut(addr)?;
        let id = *waiters.keys().next()?;
        let waker = waiters.remove(&id);
        if waiters.is_empty() {
            self.waiters.accepts.remove(addr);
        }
        waker.map(|waker| (id, waker))
    }

    pub(crate) fn peer_address(&self, id: ConnectionId) -> Option<String> {
        self.state
            .connections
            .get(&id)
            .map(|c| c.peer_address.clone())
    }

    pub(crate) fn before_event(&mut self, now: Duration) -> (NetworkActions, WakeBatch) {
        let mut wakes = WakeBatch::default();
        self.clear_expired_write_clogs(now, &mut wakes);
        let actions = self.randomly_trigger_partitions(now);
        (actions, wakes)
    }

    pub(crate) fn handle_event(
        &mut self,
        event: NetworkEvent,
        now: Duration,
    ) -> (NetworkActions, WakeBatch) {
        let mut actions = NetworkActions::default();
        let mut wakes = WakeBatch::default();
        match event {
            NetworkEvent::Maintenance => {}
            NetworkEvent::OperationReady { operation_id } => {
                self.completed_operations.insert(operation_id);
                wakes.push(self.operation_waiters.take(&operation_id));
            }
            NetworkEvent::ClogClear {
                connection_id,
                expected_deadline,
            } => {
                if self
                    .state
                    .connection_clogs
                    .get(&connection_id)
                    .is_some_and(|state| {
                        state.expires_at == expected_deadline && now >= expected_deadline
                    })
                {
                    self.state.connection_clogs.remove(&connection_id);
                    Self::take_waiter(&mut self.waiters.write_clogs, connection_id, &mut wakes);
                }
            }
            NetworkEvent::ReadClogClear {
                connection_id,
                expected_deadline,
            } => {
                if self
                    .state
                    .read_clogs
                    .get(&connection_id)
                    .is_some_and(|state| {
                        state.expires_at == expected_deadline && now >= expected_deadline
                    })
                {
                    self.state.read_clogs.remove(&connection_id);
                    Self::take_waiter(&mut self.waiters.read_clogs, connection_id, &mut wakes);
                }
            }
            NetworkEvent::PartitionRestore { expected_deadline } => {
                self.state.ip_partitions.retain(|_, state| {
                    state.expires_at != expected_deadline || now < expected_deadline
                });
                self.resume_stalled_sends(now, &mut actions);
            }
            NetworkEvent::SendPartitionClear { expected_deadline } => {
                self.state.send_partitions.retain(|_, deadline| {
                    *deadline != expected_deadline || now < expected_deadline
                });
                self.resume_stalled_sends(now, &mut actions);
            }
            NetworkEvent::RecvPartitionClear { expected_deadline } => {
                self.state.recv_partitions.retain(|_, deadline| {
                    *deadline != expected_deadline || now < expected_deadline
                });
                self.resume_stalled_sends(now, &mut actions);
            }
            NetworkEvent::DataDelivery {
                connection_id,
                data,
            } => {
                self.handle_data_delivery(connection_id, &data, now, &mut actions, &mut wakes);
            }
            NetworkEvent::ProcessSendBuffer { connection_id } => {
                self.handle_process_send_buffer(connection_id, now, &mut actions, &mut wakes);
            }
            NetworkEvent::FinDelivery { connection_id } => {
                self.handle_fin_delivery(connection_id, &mut wakes);
            }
        }
        (actions, wakes)
    }

    fn take_waiter(
        registered: &mut WakerRegistry<ConnectionId>,
        id: ConnectionId,
        wakes: &mut WakeBatch,
    ) {
        wakes.push(registered.take(&id));
    }

    fn clear_expired_write_clogs(&mut self, now: Duration, wakes: &mut WakeBatch) {
        let expired = self
            .state
            .connection_clogs
            .iter()
            .filter_map(|(id, state)| (now >= state.expires_at).then_some(*id))
            .collect::<Vec<_>>();
        for id in expired {
            self.state.connection_clogs.remove(&id);
            Self::take_waiter(&mut self.waiters.write_clogs, id, wakes);
        }
    }

    fn handle_data_delivery(
        &mut self,
        id: ConnectionId,
        data: &[u8],
        now: Duration,
        actions: &mut NetworkActions,
        wakes: &mut WakeBatch,
    ) {
        if !self.state.connections.get(&id).is_some_and(|connection| {
            !connection.flags.is_closed() && !connection.flags.recv_closed()
        }) {
            return;
        }
        // The chunk left a black-holed sender: it was acknowledged into that
        // side's buffer and is gone. No bytes, no wake — the reader keeps
        // waiting for data that will never come.
        if self.peer_send_black_holed(id) {
            return;
        }
        let delivered = self.maybe_corrupt_data(id, data, now, actions);
        if let Some(connection) = self.state.connections.get_mut(&id) {
            connection.receive_buffer.extend(delivered);
        }
        wakes.push(self.waiters.reads.take(&id));
    }

    fn handle_fin_delivery(&mut self, id: ConnectionId, wakes: &mut WakeBatch) {
        // A FIN is a send like any other: from a black-holed peer it vanishes,
        // and the reader never sees EOF.
        if self.peer_send_black_holed(id) {
            return;
        }
        if let Some(connection) = self.state.connections.get_mut(&id)
            && !connection.flags.is_closed()
        {
            connection.flags.set_remote_fin_received(true);
            wakes.push(self.waiters.reads.take(&id));
        }
    }

    /// Whether the endpoint that sends *to* `id` has its sends black-holed.
    fn peer_send_black_holed(&self, id: ConnectionId) -> bool {
        self.state
            .connections
            .get(&id)
            .and_then(|connection| connection.paired_connection)
            .and_then(|peer| self.state.connections.get(&peer))
            .is_some_and(|peer| peer.flags.send_black_holed())
    }

    fn calculate_flip_bit_count(random_value: u32, min_bits: u32, max_bits: u32) -> u32 {
        if random_value == 0 {
            return max_bits.min(32);
        }
        (1 + random_value.leading_zeros()).clamp(min_bits, max_bits)
    }

    fn maybe_corrupt_data(
        &mut self,
        id: ConnectionId,
        data: &[u8],
        now: Duration,
        actions: &mut NetworkActions,
    ) -> Vec<u8> {
        if data.is_empty() {
            return Vec::new();
        }
        let chaos = &self.state.config.chaos;
        if now.saturating_sub(self.last_bit_flip_time) < chaos.bit_flip_cooldown
            || !crate::buggify_with_prob!(chaos.bit_flip_probability)
        {
            return data.to_vec();
        }
        let count = Self::calculate_flip_bit_count(
            sim_random::<u32>(),
            chaos.bit_flip_min_bits,
            chaos.bit_flip_max_bits,
        );
        let mut result = data.to_vec();
        let mut positions = BTreeSet::new();
        for _ in 0..count {
            let raw_byte = sim_random::<u64>();
            let raw_bit = sim_random::<u64>();
            let len = u64::try_from(result.len()).expect("buffer length fits in u64");
            let byte = usize::try_from(raw_byte % len).expect("index is bounded by buffer length");
            let bit = usize::try_from(raw_bit % 8).expect("bit index is below eight");
            if positions.insert((byte, bit)) {
                result[byte] ^= 1 << bit;
            }
        }
        self.last_bit_flip_time = now;
        actions.record(SimFaultEvent::BitFlip {
            connection_id: id.0,
            flip_count: positions.len(),
        });
        result
    }

    fn handle_process_send_buffer(
        &mut self,
        id: ConnectionId,
        now: Duration,
        actions: &mut NetworkActions,
        wakes: &mut WakeBatch,
    ) {
        if self.state.connections.get(&id).is_none_or(|connection| {
            (connection.flags.is_closed() || connection.flags.send_closed())
                && !connection.flags.graceful_close_pending()
        }) {
            if let Some(connection) = self.state.connections.get_mut(&id) {
                connection.send_buffer.clear();
                connection.flags.set_send_in_progress(false);
                connection.flags.set_send_stalled(false);
            }
            Self::take_waiter(&mut self.waiters.send_buffers, id, wakes);
            return;
        }
        // Only queued bytes can stall: with nothing to send there is nothing a
        // partition could reorder, and the normal path is what releases the
        // send-in-progress flag and any pending FIN.
        let has_queued_bytes = self
            .state
            .connections
            .get(&id)
            .is_some_and(|connection| !connection.send_buffer.is_empty());
        if has_queued_bytes && self.state.connection_partition_clear_at(id, now).is_some() {
            self.stall_partitioned_send(id);
        } else {
            self.handle_normal_send(id, now, actions, wakes);
        }
    }

    /// Hold a queued send until every partition blocking it has healed.
    ///
    /// A partition must never punch a hole in an established byte stream. The
    /// queued chunk stays at the front of the send buffer, so the peer either
    /// sees the original bytes in order once the partition heals, or sees the
    /// connection fail — never a later chunk silently filling the gap left by
    /// an earlier one. `FoundationDB` models the same thing: `SimClogging` turns
    /// a clogged pair into added delay (`getRecvDelay` clamps to
    /// `clogPairUntil`), and only an explicit disconnect fails the connection.
    ///
    /// Send-buffer waiters are deliberately left registered: no buffer space is
    /// released while the stream is stalled, so writers keep seeing
    /// backpressure until the send actually drains.
    ///
    /// A stalled connection owns no scheduled work. It is re-driven by
    /// [`resume_stalled_sends`](Self::resume_stalled_sends) when the partitions
    /// blocking it heal, whether that happens at their deadline or earlier.
    fn stall_partitioned_send(&mut self, id: ConnectionId) {
        if let Some(connection) = self.state.connections.get_mut(&id) {
            connection.flags.set_send_stalled(true);
        }
    }

    /// Re-drive every connection whose blocking partitions have healed.
    ///
    /// Runs from each partition-clearing path, so a stream stalled by a
    /// partition that is healed early releases its bytes early instead of
    /// waiting out the deadline it stalled under.
    fn resume_stalled_sends(&mut self, now: Duration, actions: &mut NetworkActions) {
        let resumed = self
            .state
            .connections
            .iter()
            .filter(|(id, connection)| {
                connection.flags.send_stalled()
                    && self
                        .state
                        .connection_partition_clear_at(**id, now)
                        .is_none()
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in resumed {
            if let Some(connection) = self.state.connections.get_mut(&id) {
                connection.flags.set_send_stalled(false);
            }
            actions.schedule_at(now, NetworkEvent::ProcessSendBuffer { connection_id: id });
        }
    }

    fn handle_normal_send(
        &mut self,
        id: ConnectionId,
        now: Duration,
        actions: &mut NetworkActions,
        wakes: &mut WakeBatch,
    ) {
        let Some(snapshot) = self.state.connections.get(&id).map(|connection| {
            (
                connection.paired_connection,
                connection.next_send_time,
                connection.local_ip,
                connection.remote_ip,
            )
        }) else {
            return;
        };
        let (paired_id, next_send_time, local_ip, remote_ip) = snapshot;
        let pair_extra = local_ip
            .zip(remote_ip)
            .and_then(|pair| self.state.pair_latencies.get(&pair).copied())
            .unwrap_or(Duration::ZERO);
        let partial_max = self.state.config.chaos.partial_write_max_bytes;
        let write_latency = self.state.config.write_latency.clone();
        let Some(connection) = self.state.connections.get_mut(&id) else {
            return;
        };
        let Some(mut data) = connection.send_buffer.pop_front() else {
            connection.flags.set_send_in_progress(false);
            if connection.flags.graceful_close_pending() {
                connection.flags.set_graceful_close_pending(false);
                Self::schedule_fin(
                    connection.paired_connection,
                    connection.last_data_delivery_scheduled_at,
                    now,
                    actions,
                );
            }
            return;
        };
        Self::take_waiter(&mut self.waiters.send_buffers, id, wakes);
        if crate::buggify!() && !data.is_empty() {
            let max_send = data.len().min(partial_max);
            let truncate_to = sim_random_range(0..max_send + 1);
            if truncate_to < data.len() {
                connection
                    .send_buffer
                    .push_front(data.split_off(truncate_to));
            }
        }
        let base_delay = if connection.send_buffer.is_empty() {
            crate::network::sample_latency(&write_latency)
        } else {
            Duration::from_nanos(1)
        };
        let earliest = now.saturating_add(base_delay).max(next_send_time);
        connection.next_send_time = earliest.saturating_add(Duration::from_nanos(1));
        if let Some(paired) = paired_id {
            let at = earliest.saturating_add(pair_extra);
            actions.schedule_at(
                at,
                NetworkEvent::DataDelivery {
                    connection_id: paired,
                    data,
                },
            );
            connection.last_data_delivery_scheduled_at = Some(at);
        }
        if connection.send_buffer.is_empty() {
            connection.flags.set_send_in_progress(false);
            if connection.flags.graceful_close_pending() {
                connection.flags.set_graceful_close_pending(false);
                Self::schedule_fin(
                    connection.paired_connection,
                    connection.last_data_delivery_scheduled_at,
                    now,
                    actions,
                );
            }
        } else {
            actions.schedule_at(now, NetworkEvent::ProcessSendBuffer { connection_id: id });
        }
    }

    fn schedule_fin(
        paired: Option<ConnectionId>,
        last_delivery: Option<Duration>,
        now: Duration,
        actions: &mut NetworkActions,
    ) {
        let Some(connection_id) = paired else {
            return;
        };
        let at = last_delivery
            .filter(|time| *time >= now)
            .unwrap_or(now)
            .saturating_add(Duration::from_nanos(1));
        actions.schedule_at(at, NetworkEvent::FinDelivery { connection_id });
    }

    pub(crate) fn shutdown_waiters(&mut self) -> WakeBatch {
        let mut wakes = WakeBatch::default();
        for waiters in std::mem::take(&mut self.waiters.accepts).into_values() {
            for (id, waker) in waiters {
                self.failed_accepts.insert(id);
                wakes.extend([waker]);
            }
        }
        for (id, reservation) in std::mem::take(&mut self.accept_reservations) {
            self.failed_accepts.insert(id);
            wakes.extend([reservation.waker]);
        }
        self.state.pending_connections.clear();
        let connections = self.state.connections.keys().copied().collect::<Vec<_>>();
        for connection in connections {
            wakes.append(self.close_aborted(connection));
        }
        wakes.extend(self.waiters.reads.drain().map(|(_, waker)| waker));
        for waiters in [
            &mut self.waiters.write_clogs,
            &mut self.waiters.read_clogs,
            &mut self.waiters.send_buffers,
        ] {
            wakes.extend(waiters.drain().map(|(_, waker)| waker));
        }
        for operation in std::mem::take(&mut self.completed_operations) {
            self.failed_operations.insert(operation);
        }
        for (operation, waker) in self.operation_waiters.drain() {
            self.failed_operations.insert(operation);
            wakes.extend([waker]);
        }
        wakes
    }

    pub(crate) fn allocate_operation(&mut self) -> Option<NetworkOperationId> {
        let id = NetworkOperationId(self.next_operation_id);
        self.next_operation_id = self.next_operation_id.checked_add(1)?;
        Some(id)
    }

    pub(crate) fn poll_operation(
        &mut self,
        id: NetworkOperationId,
        waker: &Waker,
    ) -> SimulationResult<bool> {
        if self.failed_operations.remove(&id) {
            self.operation_waiters.take(&id);
            return Err(SimulationError::SimulationShutdown);
        }
        if self.completed_operations.remove(&id) {
            self.operation_waiters.take(&id);
            Ok(true)
        } else {
            self.operation_waiters.register(id, waker);
            Ok(false)
        }
    }

    pub(crate) fn cancel_operation(&mut self, id: NetworkOperationId) {
        self.completed_operations.remove(&id);
        self.failed_operations.remove(&id);
        self.operation_waiters.take(&id);
    }

    pub(crate) fn fail_operation(&mut self, id: NetworkOperationId) {
        self.completed_operations.remove(&id);
        self.failed_operations.insert(id);
    }

    // Control/query methods are kept on the engine so `SimWorld` remains a lock wrapper.
    pub(crate) fn is_closed(&self, id: ConnectionId) -> bool {
        self.state
            .connections
            .get(&id)
            .is_some_and(|c| c.flags.is_closed())
    }

    pub(crate) fn close_reason(&self, id: ConnectionId) -> CloseReason {
        self.state
            .connections
            .get(&id)
            .map_or(CloseReason::None, |c| c.close_reason)
    }

    pub(crate) fn is_send_closed(&self, id: ConnectionId) -> bool {
        self.state
            .connections
            .get(&id)
            .is_some_and(|c| c.flags.send_closed() || c.flags.is_closed())
    }

    pub(crate) fn is_recv_closed(&self, id: ConnectionId) -> bool {
        self.state
            .connections
            .get(&id)
            .is_some_and(|c| c.flags.recv_closed() || c.flags.is_closed())
    }

    pub(crate) fn remote_fin_received(&self, id: ConnectionId) -> bool {
        self.state
            .connections
            .get(&id)
            .is_some_and(|c| c.flags.remote_fin_received())
    }

    pub(crate) fn send_buffer_capacity(&self, id: ConnectionId) -> usize {
        self.state
            .connections
            .get(&id)
            .map_or(0, |c| c.send_buffer_capacity)
    }

    pub(crate) fn send_buffer_used(&self, id: ConnectionId) -> usize {
        self.state
            .connections
            .get(&id)
            .map_or(0, |c| c.send_buffer.iter().map(Vec::len).sum())
    }

    pub(crate) fn register_send_buffer(&mut self, id: ConnectionId, waker: &Waker) -> bool {
        if self
            .state
            .connections
            .get(&id)
            .is_none_or(|connection| connection.flags.is_closed() || connection.flags.send_closed())
        {
            return false;
        }
        self.waiters.send_buffers.register(id, waker);
        true
    }

    pub(crate) fn pair_latency(&self, src: IpAddr, dst: IpAddr) -> Option<Duration> {
        self.state.pair_latencies.get(&(src, dst)).copied()
    }

    pub(crate) fn connection_base_latency(&mut self, id: ConnectionId) -> Duration {
        let range = self.state.config.chaos.max_pair_latency.clone();
        let link = self.state.config.link_latency.clone();
        let Some((local, remote)) = self
            .state
            .connections
            .get(&id)
            .and_then(|c| Some((c.local_ip?, c.remote_ip?)))
        else {
            return Duration::ZERO;
        };
        // A pair samples its fixed extra latency once and keeps it for the rest
        // of the run. Checking the cache *before* the configuration means
        // recovery mode (which zeroes `max_pair_latency`) stops new pairs from
        // degrading without healing a link that is already slow.
        if let Some(latency) = self.pair_latency(local, remote) {
            return latency;
        }
        if range.end.is_zero() && link.is_none() {
            return Duration::ZERO;
        }
        let mut latency = if range.end.is_zero() {
            Duration::ZERO
        } else {
            crate::network::sample_duration(&range)
        };
        if let Some(link) = &link {
            latency = latency.saturating_add(self.sample_link_latency(link, local, remote));
        }
        *self
            .state
            .pair_latencies
            .entry((local, remote))
            .or_insert(latency)
    }

    fn sample_link_latency(
        &self,
        config: &LinkLatencyConfig,
        local: IpAddr,
        remote: IpAddr,
    ) -> Duration {
        let Some(class) = self
            .localities
            .get(&local)
            .zip(self.localities.get(&remote))
            .map(|(a, b)| a.link_class(b))
        else {
            return Duration::ZERO;
        };
        crate::network::sample_latency(config.distribution_for(class))
    }

    pub(crate) fn should_clog_write(&self, id: ConnectionId, now: Duration) -> bool {
        if let Some(clog) = self.state.connection_clogs.get(&id) {
            return now < clog.expires_at;
        }
        let probability = self.state.config.chaos.clog_probability;
        probability > 0.0 && sim_random::<f64>() < probability
    }

    pub(crate) fn clog_write(&mut self, id: ConnectionId, now: Duration) -> NetworkActions {
        let duration = crate::network::sample_duration(&self.state.config.chaos.clog_duration);
        let deadline = now.saturating_add(duration);
        self.state.connection_clogs.insert(
            id,
            ClogState {
                expires_at: deadline,
            },
        );
        let mut actions = NetworkActions::default();
        actions.schedule_at(
            deadline,
            NetworkEvent::ClogClear {
                connection_id: id,
                expected_deadline: deadline,
            },
        );
        actions
    }

    pub(crate) fn is_write_clogged(&self, id: ConnectionId, now: Duration) -> bool {
        self.state
            .connection_clogs
            .get(&id)
            .is_some_and(|c| now < c.expires_at)
    }

    pub(crate) fn register_write_clog(&mut self, id: ConnectionId, waker: &Waker) -> bool {
        if self
            .state
            .connections
            .get(&id)
            .is_none_or(|connection| connection.flags.is_closed())
        {
            return false;
        }
        self.waiters.write_clogs.register(id, waker);
        true
    }

    pub(crate) fn should_clog_read(&self, id: ConnectionId, now: Duration) -> bool {
        if let Some(clog) = self.state.read_clogs.get(&id) {
            return now < clog.expires_at;
        }
        let probability = self.state.config.chaos.clog_probability;
        probability > 0.0 && sim_random::<f64>() < probability
    }

    pub(crate) fn clog_read(&mut self, id: ConnectionId, now: Duration) -> NetworkActions {
        let duration = crate::network::sample_duration(&self.state.config.chaos.clog_duration);
        let deadline = now.saturating_add(duration);
        self.state.read_clogs.insert(
            id,
            ClogState {
                expires_at: deadline,
            },
        );
        let mut actions = NetworkActions::default();
        actions.schedule_at(
            deadline,
            NetworkEvent::ReadClogClear {
                connection_id: id,
                expected_deadline: deadline,
            },
        );
        actions
    }

    pub(crate) fn is_read_clogged(&self, id: ConnectionId, now: Duration) -> bool {
        self.state
            .read_clogs
            .get(&id)
            .is_some_and(|c| now < c.expires_at)
    }

    pub(crate) fn register_read_clog(&mut self, id: ConnectionId, waker: &Waker) -> bool {
        if self
            .state
            .connections
            .get(&id)
            .is_none_or(|connection| connection.flags.is_closed())
        {
            return false;
        }
        self.waiters.read_clogs.register(id, waker);
        true
    }

    fn randomly_trigger_partitions(&mut self, now: Duration) -> NetworkActions {
        let chaos = &self.state.config.chaos;
        if chaos.partition_probability == 0.0 || sim_random::<f64>() >= chaos.partition_probability
        {
            return NetworkActions::default();
        }
        let strategy = chaos.partition_strategy;
        let duration_range = chaos.partition_duration.clone();
        let mut ips = self
            .state
            .connections
            .values()
            .filter(|connection| !connection.flags.is_closed())
            .filter_map(|connection| connection.local_ip)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        ips.sort_unstable();
        if ips.len() < 2 {
            return NetworkActions::default();
        }
        let duration = crate::network::sample_duration(&duration_range);
        if matches!(
            strategy,
            PartitionStrategy::AsymmetricSend | PartitionStrategy::AsymmetricRecv
        ) {
            let ip = ips[sim_random_range(0..ips.len())];
            return if strategy == PartitionStrategy::AsymmetricSend {
                self.insert_send_partition(ip, duration, now)
            } else {
                self.insert_recv_partition(ip, duration, now)
            };
        }
        let selected = self.select_partition_group(&ips, strategy);
        let mut actions = NetworkActions::default();
        if selected.is_empty() || selected.len() == ips.len() {
            return actions;
        }
        let deadline = now.saturating_add(duration);
        let other = ips
            .iter()
            .filter(|ip| !selected.contains(ip))
            .copied()
            .collect::<Vec<_>>();
        for from in selected {
            for &to in &other {
                if self.state.is_partitioned(from, to, now) {
                    continue;
                }
                self.state.ip_partitions.insert(
                    (from, to),
                    PartitionState {
                        expires_at: deadline,
                    },
                );
                self.state.ip_partitions.insert(
                    (to, from),
                    PartitionState {
                        expires_at: deadline,
                    },
                );
                actions.record(SimFaultEvent::PartitionCreated {
                    from: from.to_string(),
                    to: to.to_string(),
                });
            }
        }
        actions.schedule_at(
            deadline,
            NetworkEvent::PartitionRestore {
                expected_deadline: deadline,
            },
        );
        actions
    }

    fn select_partition_group(&self, ips: &[IpAddr], strategy: PartitionStrategy) -> Vec<IpAddr> {
        match strategy {
            PartitionStrategy::UniformSize => {
                let count = sim_random_range(1..ips.len());
                let mut shuffled = ips.to_vec();
                for i in (1..shuffled.len()).rev() {
                    shuffled.swap(i, sim_random_range(0..i + 1));
                }
                shuffled.into_iter().take(count).collect()
            }
            PartitionStrategy::IsolateSingle => vec![ips[sim_random_range(0..ips.len())]],
            PartitionStrategy::IsolateZone => self
                .select_domain_group(ips, DomainLevel::Zone)
                .unwrap_or_else(|| Self::select_random_group(ips)),
            PartitionStrategy::IsolateDatacenter => self
                .select_domain_group(ips, DomainLevel::Datacenter)
                .unwrap_or_else(|| Self::select_random_group(ips)),
            _ => Self::select_random_group(ips),
        }
    }

    fn select_random_group(ips: &[IpAddr]) -> Vec<IpAddr> {
        ips.iter()
            .filter(|_| sim_random::<f64>() < 0.5)
            .copied()
            .collect()
    }

    fn select_domain_group(&self, ips: &[IpAddr], level: DomainLevel) -> Option<Vec<IpAddr>> {
        let mut domains = ips
            .iter()
            .filter_map(|ip| self.localities.get(ip))
            .map(|l| l.id_for(level))
            .collect::<Vec<_>>();
        domains.sort_unstable();
        domains.dedup();
        if domains.is_empty() {
            return None;
        }
        let selected = domains[sim_random_range(0..domains.len())];
        Some(
            ips.iter()
                .filter(|ip| {
                    self.localities
                        .get(ip)
                        .is_some_and(|l| l.id_for(level) == selected)
                })
                .copied()
                .collect(),
        )
    }

    pub(crate) fn partition_pair(
        &mut self,
        from: IpAddr,
        to: IpAddr,
        duration: Duration,
        now: Duration,
    ) -> NetworkActions {
        let deadline = now.saturating_add(duration);
        self.state.ip_partitions.insert(
            (from, to),
            PartitionState {
                expires_at: deadline,
            },
        );
        let mut actions = NetworkActions::default();
        actions.schedule_at(
            deadline,
            NetworkEvent::PartitionRestore {
                expected_deadline: deadline,
            },
        );
        actions.record(SimFaultEvent::PartitionCreated {
            from: from.to_string(),
            to: to.to_string(),
        });
        actions
    }

    pub(crate) fn insert_send_partition(
        &mut self,
        ip: IpAddr,
        duration: Duration,
        now: Duration,
    ) -> NetworkActions {
        let deadline = now.saturating_add(duration);
        self.state.send_partitions.insert(ip, deadline);
        let mut actions = NetworkActions::default();
        actions.schedule_at(
            deadline,
            NetworkEvent::SendPartitionClear {
                expected_deadline: deadline,
            },
        );
        actions.record(SimFaultEvent::SendPartitionCreated { ip: ip.to_string() });
        actions
    }

    pub(crate) fn insert_recv_partition(
        &mut self,
        ip: IpAddr,
        duration: Duration,
        now: Duration,
    ) -> NetworkActions {
        let deadline = now.saturating_add(duration);
        self.state.recv_partitions.insert(ip, deadline);
        let mut actions = NetworkActions::default();
        actions.schedule_at(
            deadline,
            NetworkEvent::RecvPartitionClear {
                expected_deadline: deadline,
            },
        );
        actions.record(SimFaultEvent::RecvPartitionCreated { ip: ip.to_string() });
        actions
    }

    pub(crate) fn restore_partition(
        &mut self,
        from: IpAddr,
        to: IpAddr,
        now: Duration,
    ) -> NetworkActions {
        self.state.ip_partitions.remove(&(from, to));
        self.state.ip_partitions.remove(&(to, from));
        let mut actions = NetworkActions::default();
        actions.record(SimFaultEvent::PartitionHealed {
            from: from.to_string(),
            to: to.to_string(),
        });
        self.resume_stalled_sends(now, &mut actions);
        actions
    }

    /// Heal every environmental partition currently in force: directed pair
    /// cuts plus the send-side and receive-side blocks that
    /// [`restore_partition`](Self::restore_partition) cannot reach.
    ///
    /// Connections held back by a partition are re-driven, so a stalled send
    /// resumes instead of waiting out a deadline that no longer applies.
    pub(crate) fn heal_all_partitions(&mut self, now: Duration) -> NetworkActions {
        let mut actions = NetworkActions::default();
        for (from, to) in std::mem::take(&mut self.state.ip_partitions).into_keys() {
            actions.record(SimFaultEvent::PartitionHealed {
                from: from.to_string(),
                to: to.to_string(),
            });
        }
        for ip in std::mem::take(&mut self.state.send_partitions).into_keys() {
            actions.record(SimFaultEvent::SendPartitionHealed { ip: ip.to_string() });
        }
        for ip in std::mem::take(&mut self.state.recv_partitions).into_keys() {
            actions.record(SimFaultEvent::RecvPartitionHealed { ip: ip.to_string() });
        }
        self.resume_stalled_sends(now, &mut actions);
        actions
    }

    /// Stop sampling new network faults (see
    /// [`ChaosConfiguration::disable_fault_injection`](crate::network::ChaosConfiguration::disable_fault_injection)).
    ///
    /// Consumes no randomness and leaves every already-produced effect in
    /// place, including the per-pair latencies sampled so far.
    pub(crate) fn disable_fault_injection(&mut self) {
        self.state.config.disable_fault_injection();
    }

    pub(crate) fn is_partitioned(&self, from: IpAddr, to: IpAddr, now: Duration) -> bool {
        self.state.is_partitioned(from, to, now)
    }

    pub(crate) fn roll_random_close(
        &mut self,
        id: ConnectionId,
        now: Duration,
    ) -> (Option<bool>, NetworkActions, WakeBatch) {
        let config = &self.state.config.chaos;
        if config.random_close_probability <= 0.0
            || now.saturating_sub(self.state.last_random_close_time) < config.random_close_cooldown
            || !crate::buggify_with_prob!(config.random_close_probability)
        {
            return (None, NetworkActions::default(), WakeBatch::default());
        }
        self.state.last_random_close_time = now;
        let paired = self
            .state
            .connections
            .get(&id)
            .and_then(|c| c.paired_connection);
        let a = sim_random_f64();
        let close_recv = a < 0.66;
        let close_send = a > 0.33;
        if close_send && let Some(c) = self.state.connections.get_mut(&id) {
            c.flags.set_send_closed(true);
            c.send_buffer.clear();
        }
        if close_recv && let Some(c) = paired.and_then(|peer| self.state.connections.get_mut(&peer))
        {
            c.flags.set_recv_closed(true);
        }
        let mut wakes = WakeBatch::default();
        if close_send {
            wakes.push(self.waiters.reads.take(&id));
        }
        if close_recv && let Some(peer) = paired {
            wakes.push(self.waiters.reads.take(&peer));
        }
        let explicit = sim_random_f64() < self.state.config.chaos.random_close_explicit_ratio;
        let mut actions = NetworkActions::default();
        actions.record(SimFaultEvent::RandomClose {
            connection_id: id.0,
        });
        (Some(explicit), actions, wakes)
    }

    /// Roll the black-hole coin for one I/O on `id` (the `rollRandomClose`
    /// shape: own probability, own cooldown, one `buggify_with_prob!` draw).
    ///
    /// A hit black-holes this endpoint's sends, its peer's, or both — the same
    /// three-way direction draw a random close makes — and is recorded once as
    /// [`SimFaultEvent::BlackHole`]. Nothing is returned to the caller: the
    /// operation that drew the fault proceeds normally, which is the point.
    /// Draws nothing while the family is off.
    pub(crate) fn roll_black_hole(&mut self, id: ConnectionId, now: Duration) -> NetworkActions {
        let config = &self.state.config.chaos;
        if self
            .state
            .connections
            .get(&id)
            .is_none_or(|connection| connection.flags.is_closed())
            || config.black_hole_probability <= 0.0
            || now.saturating_sub(self.state.last_black_hole_time) < config.black_hole_cooldown
            || !crate::buggify_with_prob!(config.black_hole_probability)
        {
            return NetworkActions::default();
        }
        self.state.last_black_hole_time = now;
        let a = sim_random_f64();
        let hole_recv = a < 0.66;
        let hole_send = a > 0.33;
        assert_reachable!("network: connection black-holed");
        self.black_hole(id, hole_send, hole_recv)
    }

    /// Black-hole `id`'s sends (`hole_send`) and/or its peer's (`hole_recv`).
    ///
    /// Permanent for the connection's lifetime and idempotent per direction;
    /// the fault is recorded only when a direction that was not yet holed is.
    pub(crate) fn black_hole(
        &mut self,
        id: ConnectionId,
        hole_send: bool,
        hole_recv: bool,
    ) -> NetworkActions {
        let paired = self
            .state
            .connections
            .get(&id)
            .and_then(|c| c.paired_connection);
        let mut newly_send = false;
        if hole_send
            && let Some(c) = self.state.connections.get_mut(&id)
            && !c.flags.send_black_holed()
        {
            c.flags.set_send_black_holed(true);
            newly_send = true;
        }
        let mut newly_recv = false;
        if hole_recv
            && let Some(c) = paired.and_then(|peer| self.state.connections.get_mut(&peer))
            && !c.flags.send_black_holed()
        {
            c.flags.set_send_black_holed(true);
            newly_recv = true;
        }
        let mut actions = NetworkActions::default();
        let direction = match (newly_send, newly_recv) {
            (true, true) => "both",
            (true, false) => "send",
            (false, true) => "recv",
            (false, false) => return actions,
        };
        actions.record(SimFaultEvent::BlackHole {
            connection_id: id.0,
            direction: direction.to_string(),
        });
        actions
    }

    pub(crate) fn is_send_black_holed(&self, id: ConnectionId) -> bool {
        self.state
            .connections
            .get(&id)
            .is_some_and(|connection| connection.flags.send_black_holed())
    }

    pub(crate) fn close_graceful(
        &mut self,
        id: ConnectionId,
        now: Duration,
    ) -> (NetworkActions, WakeBatch) {
        let mut actions = NetworkActions::default();
        let mut wakes = WakeBatch::default();
        let Some(snapshot) = self.state.connections.get(&id).map(|c| {
            (
                c.paired_connection,
                c.flags.send_closed(),
                c.flags.is_closed(),
                c.flags.send_in_progress(),
                c.send_buffer.is_empty(),
                c.last_data_delivery_scheduled_at,
            )
        }) else {
            return (actions, wakes);
        };
        if snapshot.1 || snapshot.2 {
            return (actions, wakes);
        }
        if let Some(c) = self.state.connections.get_mut(&id) {
            c.flags.set_is_closed(true);
            c.flags.set_send_closed(true);
            c.close_reason = CloseReason::Graceful;
            if snapshot.3 || !snapshot.4 {
                c.flags.set_graceful_close_pending(true);
            }
        }
        wakes.push(self.waiters.reads.take(&id));
        if !snapshot.3 && snapshot.4 {
            Self::schedule_fin(snapshot.0, snapshot.5, now, &mut actions);
        }
        (actions, wakes)
    }

    pub(crate) fn close_aborted(&mut self, id: ConnectionId) -> WakeBatch {
        let paired = self
            .state
            .connections
            .get(&id)
            .and_then(|c| c.paired_connection);
        for current in [Some(id), paired].into_iter().flatten() {
            if let Some(c) = self.state.connections.get_mut(&current) {
                c.flags.set_is_closed(true);
                c.flags.set_send_closed(true);
                c.flags.set_recv_closed(true);
                c.flags.set_send_in_progress(false);
                c.flags.set_send_stalled(false);
                c.flags.set_graceful_close_pending(false);
                c.send_buffer.clear();
                c.close_reason = CloseReason::Aborted;
            }
        }
        let mut wakes = WakeBatch::default();
        for current in [Some(id), paired].into_iter().flatten() {
            wakes.push(self.waiters.reads.take(&current));
            Self::take_waiter(&mut self.waiters.write_clogs, current, &mut wakes);
            Self::take_waiter(&mut self.waiters.read_clogs, current, &mut wakes);
            Self::take_waiter(&mut self.waiters.send_buffers, current, &mut wakes);
        }
        wakes
    }

    pub(crate) fn close_asymmetric(
        &mut self,
        id: ConnectionId,
        close_send: bool,
        close_recv: bool,
    ) -> WakeBatch {
        let paired = self
            .state
            .connections
            .get(&id)
            .and_then(|c| c.paired_connection);
        if close_send && let Some(c) = self.state.connections.get_mut(&id) {
            c.flags.set_send_closed(true);
            c.send_buffer.clear();
        }
        if close_recv && let Some(c) = paired.and_then(|peer| self.state.connections.get_mut(&peer))
        {
            c.flags.set_recv_closed(true);
        }
        let mut wakes = WakeBatch::default();
        if close_send {
            wakes.push(self.waiters.reads.take(&id));
        }
        if close_recv && let Some(peer) = paired {
            wakes.push(self.waiters.reads.take(&peer));
        }
        wakes
    }

    pub(crate) fn connections_for_ip(&self, ip: IpAddr) -> Vec<ConnectionId> {
        self.state
            .connections
            .iter()
            .filter_map(|(id, c)| {
                (c.local_ip == Some(ip) || c.remote_ip == Some(ip)).then_some(*id)
            })
            .collect()
    }
}
