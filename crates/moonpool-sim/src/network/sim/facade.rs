//! Thin locked facade over the scheduler-independent network engine.

use std::{collections::BTreeMap, net::IpAddr, task::Waker, time::Duration};

use crate::{
    LocalityInfo, NetworkConfiguration, SimulationError, SimulationResult,
    network::sim::{
        AcceptWaiterId, CloseReason, ConnectionId, ListenerId, NetworkDelay, NetworkEvent,
        NetworkOperationId,
    },
    sim::{Event, ScheduleId, SimWorld, wakers::WakeBatch},
};

impl SimWorld {
    /// Installs process localities in the network engine.
    pub fn set_localities(&mut self, localities: BTreeMap<IpAddr, LocalityInfo>) {
        self.inner.write().network.set_localities(localities);
    }

    /// Returns locality for an IP.
    #[must_use]
    pub fn locality_for(&self, ip: IpAddr) -> Option<LocalityInfo> {
        self.inner.read().network.locality_for(ip)
    }

    /// Borrows the network configuration.
    pub fn with_network_config<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&NetworkConfiguration) -> R,
    {
        f(self.inner.read().network.config())
    }

    /// Replaces the network configuration without discarding live connections.
    ///
    /// After [`enter_recovery_mode`](Self::enter_recovery_mode) the chaos
    /// families in `config` are stripped before it is installed, so the
    /// no-new-faults promise survives a later reconfiguration. Latency
    /// distributions and link shaping are installed as given.
    pub fn set_network_config(&mut self, mut config: NetworkConfiguration) {
        let mut inner = self.inner.write();
        if inner.recovery_mode() {
            config.disable_fault_injection();
        }
        let now = inner.now();
        let actions = inner.network.set_config(config, now);
        inner.apply_network(actions);
    }

    pub(crate) fn create_listener(&self) -> ListenerId {
        self.inner.write().network.create_listener()
    }

    pub(crate) fn read_from_connection(
        &self,
        id: ConnectionId,
        buf: &mut [u8],
    ) -> SimulationResult<usize> {
        self.inner.write().network.read(id, buf)
    }

    pub(crate) fn has_readable_data(&self, id: ConnectionId) -> bool {
        self.inner.read().network.has_readable_data(id)
    }

    pub(crate) fn buffer_send(&self, id: ConnectionId, data: Vec<u8>) -> SimulationResult<()> {
        let mut inner = self.inner.write();
        let now = inner.now();
        let actions = inner.network.buffer_send(id, data, now)?;
        inner.apply_network(actions);
        Ok(())
    }

    pub(crate) fn create_connection_pair(
        &self,
        client: &str,
        server: &str,
    ) -> (ConnectionId, ConnectionId) {
        let mut inner = self.inner.write();
        let now = inner.now();
        inner.network.create_connection_pair(client, server, now)
    }

    pub(crate) fn discard_connection_pair(&self, id: ConnectionId) {
        let wakes = self.inner.write().network.discard_connection_pair(id);
        wakes.wake();
    }

    pub(crate) fn register_read_waker(&self, id: ConnectionId, waker: Waker) -> bool {
        self.inner.write().network.register_read(id, waker)
    }

    pub(crate) fn allocate_accept_waiter(&self) -> SimulationResult<AcceptWaiterId> {
        self.inner
            .write()
            .network
            .allocate_accept_waiter()
            .ok_or_else(|| {
                SimulationError::InvalidState(
                    "accept waiter identifier space exhausted".to_string(),
                )
            })
    }

    pub(crate) fn poll_accept(
        &self,
        addr: &str,
        id: AcceptWaiterId,
        waker: Waker,
    ) -> SimulationResult<Option<ConnectionId>> {
        self.inner.write().network.poll_accept(addr, id, waker)
    }

    pub(crate) fn cancel_accept(&self, addr: &str, id: AcceptWaiterId) {
        let wakes = self.inner.write().network.cancel_accept(addr, id);
        wakes.wake();
    }

    pub(crate) fn store_pending_connection(&self, addr: &str, id: ConnectionId) {
        let wakes = self.inner.write().network.store_pending(addr, id);
        wakes.wake();
    }

    pub(crate) fn return_pending_connection(&self, addr: &str, id: ConnectionId) {
        let wakes = self
            .inner
            .write()
            .network
            .return_pending_connection(addr, id);
        wakes.wake();
    }

    pub(crate) fn connection_peer_address(&self, id: ConnectionId) -> Option<String> {
        self.inner.read().network.peer_address(id)
    }

    pub(crate) fn network_delay(&self, delay: Duration) -> SimulationResult<NetworkDelay> {
        let mut inner = self.inner.write();
        let operation_id = inner.network.allocate_operation().ok_or_else(|| {
            SimulationError::InvalidState(
                "network operation identifier space exhausted".to_string(),
            )
        })?;
        let event = Event::Network(NetworkEvent::OperationReady { operation_id });
        match inner.scheduler.schedule_after(delay, event) {
            Ok(schedule_id) => {
                inner.network_schedules.insert(operation_id, schedule_id);
                Ok(NetworkDelay::new(
                    self.downgrade(),
                    operation_id,
                    schedule_id,
                ))
            }
            Err(error) => {
                inner.network.cancel_operation(operation_id);
                Err(SimulationError::InvalidState(error.to_string()))
            }
        }
    }

    pub(crate) fn poll_network_operation(
        &self,
        operation_id: NetworkOperationId,
        waker: &Waker,
    ) -> SimulationResult<bool> {
        self.inner
            .write()
            .network
            .poll_operation(operation_id, waker)
    }

    pub(crate) fn cancel_network_operation(
        &self,
        operation_id: NetworkOperationId,
        schedule_id: ScheduleId,
    ) {
        let mut inner = self.inner.write();
        inner.scheduler.cancel(schedule_id);
        inner.network_schedules.remove(&operation_id);
        inner.network.cancel_operation(operation_id);
    }

    /// Returns whether write clog chaos should trigger.
    #[must_use]
    pub fn should_clog_write(&self, id: ConnectionId) -> bool {
        let inner = self.inner.read();
        inner.network.should_clog_write(id, inner.now())
    }

    /// Starts a write clog.
    pub fn clog_write(&self, id: ConnectionId) {
        let mut inner = self.inner.write();
        let now = inner.now();
        let actions = inner.network.clog_write(id, now);
        inner.apply_network(actions);
    }

    /// Returns whether a write is clogged.
    #[must_use]
    pub fn is_write_clogged(&self, id: ConnectionId) -> bool {
        let inner = self.inner.read();
        inner.network.is_write_clogged(id, inner.now())
    }

    pub(crate) fn register_clog_waker(&self, id: ConnectionId, waker: Waker) -> bool {
        self.inner.write().network.register_write_clog(id, waker)
    }

    /// Returns whether read clog chaos should trigger.
    #[must_use]
    pub fn should_clog_read(&self, id: ConnectionId) -> bool {
        let inner = self.inner.read();
        inner.network.should_clog_read(id, inner.now())
    }

    /// Starts a read clog.
    pub fn clog_read(&self, id: ConnectionId) {
        let mut inner = self.inner.write();
        let now = inner.now();
        let actions = inner.network.clog_read(id, now);
        inner.apply_network(actions);
    }

    /// Returns whether a read is clogged.
    #[must_use]
    pub fn is_read_clogged(&self, id: ConnectionId) -> bool {
        let inner = self.inner.read();
        inner.network.is_read_clogged(id, inner.now())
    }

    pub(crate) fn register_read_clog_waker(&self, id: ConnectionId, waker: Waker) -> bool {
        self.inner.write().network.register_read_clog(id, waker)
    }

    /// Clears expired write clogs.
    pub fn clear_expired_clogs(&self) {
        let wakes = {
            let mut inner = self.inner.write();
            let now = inner.now();
            inner.network.clear_expired_clogs(now)
        };
        wakes.wake();
    }

    /// Returns send-buffer capacity.
    #[must_use]
    pub fn send_buffer_capacity(&self, id: ConnectionId) -> usize {
        self.inner.read().network.send_buffer_capacity(id)
    }

    /// Returns used send-buffer bytes.
    #[must_use]
    pub fn send_buffer_used(&self, id: ConnectionId) -> usize {
        self.inner.read().network.send_buffer_used(id)
    }

    /// Returns available send-buffer bytes.
    #[must_use]
    pub fn available_send_buffer(&self, id: ConnectionId) -> usize {
        self.send_buffer_capacity(id)
            .saturating_sub(self.send_buffer_used(id))
    }

    pub(crate) fn register_send_buffer_waker(&self, id: ConnectionId, waker: Waker) -> bool {
        self.inner.write().network.register_send_buffer(id, waker)
    }

    /// Returns fixed latency for a directed IP pair.
    #[must_use]
    pub fn pair_latency(&self, src: IpAddr, dst: IpAddr) -> Option<Duration> {
        self.inner.read().network.pair_latency(src, dst)
    }

    /// Returns or samples the base latency for a connection.
    #[must_use]
    pub fn connection_base_latency(&self, id: ConnectionId) -> Duration {
        self.inner.write().network.connection_base_latency(id)
    }

    /// Returns a connection send delay override.
    #[must_use]
    pub fn send_delay(&self, id: ConnectionId) -> Option<Duration> {
        self.inner.read().network.send_delay(id)
    }

    /// Returns whether a connection is closed.
    #[must_use]
    pub fn is_connection_closed(&self, id: ConnectionId) -> bool {
        self.inner.read().network.is_closed(id)
    }

    /// Returns a connection close reason.
    #[must_use]
    pub fn close_reason(&self, id: ConnectionId) -> CloseReason {
        self.inner.read().network.close_reason(id)
    }

    /// Gracefully closes a connection.
    pub fn close_connection(&self, id: ConnectionId) {
        let wakes = {
            let mut inner = self.inner.write();
            let now = inner.now();
            let (actions, wakes) = inner.network.close_graceful(id, now);
            inner.apply_network(actions);
            wakes
        };
        wakes.wake();
    }

    /// Aborts a connection with RST semantics.
    pub fn close_connection_abort(&self, id: ConnectionId) {
        let wakes = self.inner.write().network.close_aborted(id);
        wakes.wake();
    }

    /// Closes selected directions of a connection.
    pub fn close_connection_asymmetric(
        &self,
        id: ConnectionId,
        close_send: bool,
        close_recv: bool,
    ) {
        let wakes = self
            .inner
            .write()
            .network
            .close_asymmetric(id, close_send, close_recv);
        wakes.wake();
    }

    /// Injects a random asymmetric close when configured.
    #[must_use]
    pub fn roll_random_close(&self, id: ConnectionId) -> Option<bool> {
        let (result, wakes) = {
            let mut inner = self.inner.write();
            let now = inner.now();
            let (result, actions, wakes) = inner.network.roll_random_close(id, now);
            inner.apply_network(actions);
            (result, wakes)
        };
        wakes.wake();
        result
    }

    /// Returns whether the send side is closed.
    #[must_use]
    pub fn is_send_closed(&self, id: ConnectionId) -> bool {
        self.inner.read().network.is_send_closed(id)
    }

    /// Returns whether the receive side is closed.
    #[must_use]
    pub fn is_recv_closed(&self, id: ConnectionId) -> bool {
        self.inner.read().network.is_recv_closed(id)
    }

    /// Returns whether the remote FIN arrived.
    #[must_use]
    pub fn is_remote_fin_received(&self, id: ConnectionId) -> bool {
        self.inner.read().network.remote_fin_received(id)
    }

    /// Exempts a connection pair from network chaos.
    pub fn mark_connection_stable(&self, id: ConnectionId) {
        self.inner.write().network.mark_stable(id);
    }

    /// Creates a directed pair partition.
    pub fn partition_pair(&self, from: IpAddr, to: IpAddr, duration: Duration) {
        let mut inner = self.inner.write();
        let now = inner.now();
        let actions = inner.network.partition_pair(from, to, duration, now);
        inner.apply_network(actions);
    }

    /// Blocks all sends from an IP.
    pub fn partition_send_from(&self, ip: IpAddr, duration: Duration) {
        let mut inner = self.inner.write();
        let now = inner.now();
        let actions = inner.network.insert_send_partition(ip, duration, now);
        inner.apply_network(actions);
    }

    /// Blocks all receives to an IP.
    pub fn partition_recv_to(&self, ip: IpAddr, duration: Duration) {
        let mut inner = self.inner.write();
        let now = inner.now();
        let actions = inner.network.insert_recv_partition(ip, duration, now);
        inner.apply_network(actions);
    }

    /// Restores pair partitions in both directions between two IPs.
    pub fn restore_partition(&self, from: IpAddr, to: IpAddr) {
        let mut inner = self.inner.write();
        let now = inner.now();
        let actions = inner.network.restore_partition(from, to, now);
        inner.apply_network(actions);
    }

    /// Returns whether a directed pair is partitioned.
    #[must_use]
    pub fn is_partitioned(&self, from: IpAddr, to: IpAddr) -> bool {
        let inner = self.inner.read();
        inner.network.is_partitioned(from, to, inner.now())
    }

    /// Aborts every connection involving an IP.
    pub fn abort_all_connections_for_ip(&self, ip: IpAddr) {
        let wakes = {
            let mut inner = self.inner.write();
            let ids = inner.network.connections_for_ip(ip);
            let mut wakes = WakeBatch::default();
            for id in ids {
                wakes.append(inner.network.close_aborted(id));
            }
            wakes
        };
        wakes.wake();
    }
}
