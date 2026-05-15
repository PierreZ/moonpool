//! Connection-state accessors: send buffer, latency, asymmetric delays,
//! send/recv close state, half-open, and stable-connection flags.
//!
//! These are read-mostly inherent methods on [`SimWorld`]. Close,
//! dispatch, and data-delivery logic stay in `world.rs` because they
//! are tightly coupled with the event loop.

use std::{net::IpAddr, task::Waker, time::Duration};

use crate::{
    chaos::fault_events::SimFaultEvent,
    network::sim::ConnectionId,
    sim::{
        events::{ConnectionStateChange, Event, ScheduledEvent},
        world::SimWorld,
    },
};

impl SimWorld {
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

    /// Get the base latency for a connection pair, if already set.
    pub fn pair_latency(&self, src: IpAddr, dst: IpAddr) -> Option<Duration> {
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
    pub fn connection_base_latency(&self, connection_id: ConnectionId) -> Duration {
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

        if let Some(latency) = self.pair_latency(local_ip, remote_ip) {
            return latency;
        }

        let latency = self
            .with_network_config(|config| crate::network::sample_duration(&config.write_latency));
        self.set_pair_latency_if_not_set(local_ip, remote_ip, latency)
    }

    /// Get the per-connection send delay override, if set.
    pub fn send_delay(&self, connection_id: ConnectionId) -> Option<Duration> {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .and_then(|conn| conn.send_delay)
    }

    /// Get the per-connection recv delay override, if set.
    pub fn recv_delay(&self, connection_id: ConnectionId) -> Option<Duration> {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .and_then(|conn| conn.recv_delay)
    }

    /// Set asymmetric delays for a connection. `None` falls back to global config.
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

    /// Check if a connection's send side is closed.
    pub fn is_send_closed(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.send_closed || conn.is_closed)
    }

    /// Check if a connection's receive side is closed.
    pub fn is_recv_closed(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.recv_closed || conn.is_closed)
    }

    /// Check if a FIN has been received from the remote peer (graceful close).
    ///
    /// When true, `poll_read` should return EOF after draining the receive buffer.
    /// Distinct from `is_recv_closed` which is used for chaos/asymmetric closure.
    pub fn is_remote_fin_received(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.remote_fin_received)
    }

    /// Simulate a peer crash on a connection.
    ///
    /// Puts the connection in a half-open state where:
    /// - The local side still thinks it's connected
    /// - Writes succeed but data is silently discarded (peer is gone)
    /// - Reads block waiting for data that will never come
    /// - After `error_delay`, both read and write return ECONNRESET
    pub fn simulate_peer_crash(&self, connection_id: ConnectionId, error_delay: Duration) {
        let mut inner = self.inner.borrow_mut();
        let current_time = inner.current_time;
        let error_at = current_time + error_delay;

        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            conn.is_half_open = true;
            conn.half_open_error_at = Some(error_at);
            conn.paired_connection = None;

            inner.emit_fault(SimFaultEvent::PeerCrash {
                connection_id: connection_id.0,
            });

            tracing::info!(
                "Connection {} now half-open, errors manifest at {:?}",
                connection_id.0,
                error_at
            );
        }

        let wake_event = Event::Connection {
            id: connection_id.0,
            state: ConnectionStateChange::HalfOpenError,
        };
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        let scheduled_event = ScheduledEvent::new(error_at, wake_event, sequence);
        inner.event_queue.schedule(scheduled_event);
    }

    /// Check if a connection is in a half-open state.
    pub fn is_half_open(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.is_half_open)
    }

    /// Check if a half-open connection should return errors now.
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

    /// Mark a connection as stable, exempting it from chaos injection.
    ///
    /// Stable connections are exempt from random close, clogging,
    /// bit-flip corruption, and partial write truncation.
    ///
    /// FDB ref: sim2.actor.cpp:357-362 (`stableConnection` flag).
    pub fn mark_connection_stable(&self, connection_id: ConnectionId) {
        let mut inner = self.inner.borrow_mut();
        if let Some(conn) = inner.network.connections.get_mut(&connection_id) {
            conn.is_stable = true;
            tracing::debug!("Connection {} marked as stable", connection_id.0);

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
}
