//! Thin locked facade over the scheduler-independent network engine.

use std::{collections::BTreeMap, io, net::IpAddr, task::Waker, time::Duration};
use tracing::instrument;

use crate::{
    LocalityInfo, NetworkConfiguration, SimulationError, SimulationResult,
    network::sim::{
        AcceptWaiterId, CloseReason, ConnectWaiterId, ConnectionId, ListenerId, NetworkActions,
        NetworkDelay, NetworkEvent, NetworkOperationId, PendingPublish,
    },
    sim::{Event, ScheduleId, SimWorld, wakers::WakeBatch, world::SimInner},
};

impl SimWorld {
    /// Run one timed network transition under the world lock and apply the
    /// scheduling and fault effects it returned.
    fn network_transition(
        &self,
        transition: impl FnOnce(&mut SimInner, Duration) -> NetworkActions,
    ) {
        let mut inner = self.inner.write();
        let now = inner.now();
        let actions = transition(&mut inner, now);
        inner.apply_network(actions);
    }

    /// [`network_transition`](Self::network_transition) for a transition that
    /// also releases waiters: they are woken after the lock is released.
    fn network_transition_waking(
        &self,
        transition: impl FnOnce(&mut SimInner, Duration) -> (NetworkActions, WakeBatch),
    ) {
        let wakes = {
            let mut inner = self.inner.write();
            let now = inner.now();
            let (actions, wakes) = transition(&mut inner, now);
            inner.apply_network(actions);
            wakes
        };
        wakes.wake();
    }

    /// Installs process localities in the network engine.
    #[instrument(level = "debug", skip_all)]
    pub fn set_localities(&mut self, localities: BTreeMap<IpAddr, LocalityInfo>) {
        self.inner.write().network.set_localities(localities);
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
    #[instrument(level = "debug", skip_all)]
    pub fn set_network_config(&mut self, mut config: NetworkConfiguration) {
        self.network_transition_waking(|inner, now| {
            if inner.recovery_mode() {
                config.disable_fault_injection();
            }
            inner.network.set_config(config, now)
        });
    }

    /// Bind `addr` for the process at `owner`, returning its resolved address.
    pub(crate) fn bind_listener(
        &self,
        addr: &str,
        owner: IpAddr,
    ) -> Result<(ListenerId, String), io::ErrorKind> {
        self.inner.write().network.bind_listener(addr, owner)
    }

    /// Release the address a dropped listener held.
    pub(crate) fn unbind_listener(&self, id: ListenerId) {
        let wakes = self.inner.write().network.unbind_listener(id);
        wakes.wake();
    }

    pub(crate) fn listener_matches(&self, addr: &str, id: ListenerId) -> bool {
        self.inner.read().network.listener_matches(addr, id)
    }

    pub(crate) fn read_from_connection(
        &self,
        id: ConnectionId,
        buf: &mut [u8],
    ) -> SimulationResult<usize> {
        let (result, wakes) = self.inner.write().network.read(id, buf);
        // The bytes just read return window to the peer's writer; wake it
        // outside the lock like every other waiter.
        wakes.wake();
        result
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
        self.inner
            .write()
            .network
            .create_connection_pair(client, server)
    }

    pub(crate) fn discard_connection_pair(&self, id: ConnectionId) {
        let wakes = self.inner.write().network.discard_connection_pair(id);
        wakes.wake();
    }

    pub(crate) fn register_read_waker(&self, id: ConnectionId, waker: &Waker) -> bool {
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

    pub(crate) fn complete_accept(&self, id: AcceptWaiterId) {
        let wakes = self.inner.write().network.complete_accept(id);
        wakes.wake();
    }

    pub(crate) fn refresh_accept_reservation_waker(&self, id: AcceptWaiterId, waker: Waker) {
        self.inner
            .write()
            .network
            .refresh_accept_reservation_waker(id, waker);
    }

    pub(crate) fn allocate_connect_waiter(&self) -> SimulationResult<ConnectWaiterId> {
        self.inner
            .write()
            .network
            .allocate_connect_waiter()
            .ok_or_else(|| {
                SimulationError::InvalidState(
                    "connect waiter identifier space exhausted".to_string(),
                )
            })
    }

    pub(crate) fn poll_store_pending_connection(
        &self,
        addr: &str,
        connection_id: ConnectionId,
        id: ConnectWaiterId,
        context_waker: Waker,
    ) -> PendingPublish {
        let (status, wakes) =
            self.inner
                .write()
                .network
                .poll_store_pending(addr, connection_id, id, context_waker);
        wakes.wake();
        status
    }

    pub(crate) fn cancel_connect_waiter(&self, addr: &str, id: ConnectWaiterId) {
        let wakes = self.inner.write().network.cancel_connect_waiter(addr, id);
        wakes.wake();
    }

    /// Publish an arriving connection to the listener on `addr`. Returns
    /// `false`, publishing nothing, when nobody is listening there.
    #[cfg(test)]
    pub(crate) fn store_pending_connection(&self, addr: &str, id: ConnectionId) -> bool {
        let wakes = self.inner.write().network.store_pending(addr, id);
        match wakes {
            Some(wakes) => {
                wakes.wake();
                true
            }
            None => false,
        }
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
    #[instrument(level = "trace", skip(self))]
    pub fn clog_write(&self, id: ConnectionId) {
        self.network_transition(|inner, now| inner.network.clog_write(id, now));
    }

    /// Returns whether a write is clogged.
    #[must_use]
    pub fn is_write_clogged(&self, id: ConnectionId) -> bool {
        let inner = self.inner.read();
        inner.network.is_write_clogged(id, inner.now())
    }

    pub(crate) fn register_clog_waker(&self, id: ConnectionId, waker: &Waker) -> bool {
        self.inner.write().network.register_write_clog(id, waker)
    }

    /// Returns whether read clog chaos should trigger.
    #[must_use]
    pub fn should_clog_read(&self, id: ConnectionId) -> bool {
        let inner = self.inner.read();
        inner.network.should_clog_read(id, inner.now())
    }

    /// Starts a read clog.
    #[instrument(level = "trace", skip(self))]
    pub fn clog_read(&self, id: ConnectionId) {
        self.network_transition(|inner, now| inner.network.clog_read(id, now));
    }

    /// Returns whether a read is clogged.
    #[must_use]
    pub fn is_read_clogged(&self, id: ConnectionId) -> bool {
        let inner = self.inner.read();
        inner.network.is_read_clogged(id, inner.now())
    }

    pub(crate) fn register_read_clog_waker(&self, id: ConnectionId, waker: &Waker) -> bool {
        self.inner.write().network.register_read_clog(id, waker)
    }

    /// The end-to-end byte window of `id`'s sending direction (see
    /// [`NetworkConfiguration::tcp_send_window_bytes`]).
    #[must_use]
    pub fn send_window_bytes(&self, id: ConnectionId) -> usize {
        self.inner.read().network.send_window_bytes(id)
    }

    /// Bytes `id` has written that its peer's application has not read yet,
    /// wherever they sit: queued locally, in flight, or unread at the peer.
    /// Never exceeds [`send_window_bytes`](Self::send_window_bytes).
    #[must_use]
    pub fn outstanding_send_bytes(&self, id: ConnectionId) -> usize {
        self.inner.read().network.outstanding_send_bytes(id)
    }

    /// Bytes a write on `id` may accept right now: the window minus what is
    /// outstanding. Zero means the next `poll_write` parks until the peer
    /// reads.
    #[must_use]
    pub fn available_send_bytes(&self, id: ConnectionId) -> usize {
        self.inner.read().network.available_send_bytes(id)
    }

    /// Bytes `id` has accepted but not yet put on the wire.
    #[must_use]
    pub fn queued_send_bytes(&self, id: ConnectionId) -> usize {
        self.inner.read().network.queued_send_bytes(id)
    }

    /// Bytes `id` has on the wire: sent, not yet in the peer's receive buffer.
    #[must_use]
    pub fn in_flight_bytes(&self, id: ConnectionId) -> usize {
        self.inner.read().network.in_flight_bytes(id)
    }

    /// Bytes delivered to `id` that its application has not read yet.
    #[must_use]
    pub fn unread_bytes(&self, id: ConnectionId) -> usize {
        self.inner.read().network.unread_bytes(id)
    }

    /// Whether a partition is currently freezing what `id` has in flight.
    #[must_use]
    pub fn is_in_flight_held(&self, id: ConnectionId) -> bool {
        self.inner.read().network.is_in_flight_held(id)
    }

    pub(crate) fn register_send_buffer_waker(&self, id: ConnectionId, waker: &Waker) -> bool {
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
    #[instrument(level = "debug", skip(self))]
    pub fn close_connection(&self, id: ConnectionId) {
        self.network_transition_waking(|inner, now| inner.network.close_graceful(id, now));
    }

    /// Shuts down the send direction of a connection (`shutdown(SHUT_WR)`):
    /// a FIN behind the queued bytes, the receive direction left open.
    #[instrument(level = "debug", skip(self))]
    pub fn shutdown_send(&self, id: ConnectionId) {
        self.network_transition_waking(|inner, now| inner.network.shutdown_send(id, now));
    }

    /// Aborts a connection with RST semantics.
    #[instrument(level = "debug", skip(self))]
    pub fn close_connection_abort(&self, id: ConnectionId) {
        let wakes = self.inner.write().network.close_aborted(id);
        wakes.wake();
    }

    /// Closes selected directions of a connection.
    #[instrument(level = "debug", skip(self))]
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
    #[instrument(level = "trace", skip(self))]
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

    /// Rolls the black-hole coin for one I/O on `id` (see
    /// [`ChaosConfiguration::black_hole_probability`](crate::ChaosConfiguration::black_hole_probability)).
    #[instrument(level = "trace", skip(self))]
    pub fn roll_black_hole(&self, id: ConnectionId) {
        self.network_transition(|inner, now| inner.network.roll_black_hole(id, now));
    }

    /// Black-holes selected directions of a connection: `hole_send` makes this
    /// endpoint's sends vanish, `hole_recv` its peer's. Writes keep succeeding
    /// and the peer's reads never see data or EOF; only an abort still crosses.
    ///
    /// The scripted counterpart of
    /// [`ChaosConfiguration::black_hole_probability`](crate::ChaosConfiguration::black_hole_probability);
    /// it consumes no randomness, and a black hole is permanent for the
    /// connection's lifetime.
    #[instrument(level = "debug", skip(self))]
    pub fn black_hole_connection(&self, id: ConnectionId, hole_send: bool, hole_recv: bool) {
        self.network_transition(|inner, _| inner.network.black_hole(id, hole_send, hole_recv));
    }

    /// Returns whether `id`'s sends are black-holed.
    #[must_use]
    pub fn is_send_black_holed(&self, id: ConnectionId) -> bool {
        self.inner.read().network.is_send_black_holed(id)
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

    /// Creates a directed pair partition.
    #[instrument(level = "debug", skip(self))]
    pub fn partition_pair(&self, from: IpAddr, to: IpAddr, duration: Duration) {
        self.network_transition(|inner, now| inner.network.partition_pair(from, to, duration, now));
    }

    /// Blocks all sends from an IP.
    #[instrument(level = "debug", skip(self))]
    pub fn partition_send_from(&self, ip: IpAddr, duration: Duration) {
        self.network_transition(|inner, now| {
            inner.network.insert_send_partition(ip, duration, now)
        });
    }

    /// Blocks all receives to an IP.
    #[instrument(level = "debug", skip(self))]
    pub fn partition_recv_to(&self, ip: IpAddr, duration: Duration) {
        self.network_transition(|inner, now| {
            inner.network.insert_recv_partition(ip, duration, now)
        });
    }

    /// Restores pair partitions in both directions between two IPs.
    #[instrument(level = "debug", skip(self))]
    pub fn restore_partition(&self, from: IpAddr, to: IpAddr) {
        self.network_transition(|inner, now| inner.network.restore_partition(from, to, now));
    }

    /// Returns whether a directed pair is partitioned.
    #[must_use]
    pub fn is_partitioned(&self, from: IpAddr, to: IpAddr) -> bool {
        let inner = self.inner.read();
        inner.network.is_partitioned(from, to, inner.now())
    }

    /// Aborts every connection involving an IP.
    #[instrument(level = "debug", skip(self))]
    pub fn abort_all_connections_for_ip(&self, ip: IpAddr) {
        let wakes = {
            let mut inner = self.inner.write();
            let ids = inner.network.connections_for_ip(ip);
            let mut wakes = WakeBatch::default();
            for id in ids {
                wakes.append(inner.network.close_aborted(id));
            }
            // A dead process listens on nothing: its addresses are free and
            // a connect to them is refused until something binds them again.
            wakes.append(inner.network.unbind_listeners_for_ip(ip));
            wakes
        };
        wakes.wake();
    }
}
