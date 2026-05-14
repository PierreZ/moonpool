//! Connection clog and cut chaos: write/read clogs and temporary cuts.
//!
//! Public API lives as inherent methods on [`SimWorld`]. The
//! `pub(super)` `clear_expired_clogs` helper is invoked from the
//! event-dispatch loop in world.rs.

use std::{task::Waker, time::Duration};

use crate::{
    chaos::fault_events::SimFaultEvent,
    network::sim::ConnectionId,
    sim::{
        events::{ConnectionStateChange, Event, ScheduledEvent},
        rng::sim_random,
        state::ClogState,
        world::{SimInner, SimWorld},
    },
};

impl SimWorld {
    /// Check if a write should be clogged based on probability.
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

        config.chaos.clog_probability > 0.0 && sim_random::<f64>() < config.chaos.clog_probability
    }

    /// Clog a connection's write operations.
    pub fn clog_write(&self, connection_id: ConnectionId) {
        let mut inner = self.inner.borrow_mut();
        let config = &inner.network.config;

        let clog_duration = crate::network::sample_duration(&config.chaos.clog_duration);
        let expires_at = inner.current_time + clog_duration;
        inner
            .network
            .connection_clogs
            .insert(connection_id, ClogState { expires_at });

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

    /// Check if a connection's writes are currently clogged.
    pub fn is_write_clogged(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();

        if let Some(clog_state) = inner.network.connection_clogs.get(&connection_id) {
            inner.current_time < clog_state.expires_at
        } else {
            false
        }
    }

    /// Register a waker for when write clog clears.
    pub fn register_clog_waker(&self, connection_id: ConnectionId, waker: Waker) {
        let mut inner = self.inner.borrow_mut();
        inner
            .wakers
            .clog_wakers
            .entry(connection_id)
            .or_default()
            .push(waker);
    }

    /// Check if a read should be clogged based on probability.
    pub fn should_clog_read(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();
        let config = &inner.network.config;

        if inner
            .network
            .connections
            .get(&connection_id)
            .is_some_and(|conn| conn.is_stable)
        {
            return false;
        }

        if let Some(clog_state) = inner.network.read_clogs.get(&connection_id) {
            return inner.current_time < clog_state.expires_at;
        }

        config.chaos.clog_probability > 0.0 && sim_random::<f64>() < config.chaos.clog_probability
    }

    /// Clog a connection's read operations.
    pub fn clog_read(&self, connection_id: ConnectionId) {
        let mut inner = self.inner.borrow_mut();
        let config = &inner.network.config;

        let clog_duration = crate::network::sample_duration(&config.chaos.clog_duration);
        let expires_at = inner.current_time + clog_duration;
        inner
            .network
            .read_clogs
            .insert(connection_id, ClogState { expires_at });

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

    /// Check if a connection's reads are currently clogged.
    pub fn is_read_clogged(&self, connection_id: ConnectionId) -> bool {
        let inner = self.inner.borrow();

        if let Some(clog_state) = inner.network.read_clogs.get(&connection_id) {
            inner.current_time < clog_state.expires_at
        } else {
            false
        }
    }

    /// Register a waker for when read clog clears.
    pub fn register_read_clog_waker(&self, connection_id: ConnectionId, waker: Waker) {
        let mut inner = self.inner.borrow_mut();
        inner
            .wakers
            .read_clog_wakers
            .entry(connection_id)
            .or_default()
            .push(waker);
    }

    /// Clear expired write clogs and wake pending tasks.
    pub fn clear_expired_clogs(&self) {
        let mut inner = self.inner.borrow_mut();
        clear_expired_clogs_inner(&mut inner);
    }

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

            inner.emit_fault(SimFaultEvent::ConnectionCut {
                connection_id: connection_id.0,
                duration_ms: duration.as_millis() as u64,
            });

            tracing::debug!("Connection {} cut until {:?}", connection_id.0, expires_at);

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
    /// Cancels the cut state and wakes any tasks waiting for restoration.
    pub fn restore_connection(&self, connection_id: ConnectionId) {
        let mut inner = self.inner.borrow_mut();

        if let Some(conn) = inner.network.connections.get_mut(&connection_id)
            && conn.is_cut
        {
            conn.is_cut = false;
            conn.cut_expiry = None;
            tracing::debug!("Connection {} restored", connection_id.0);

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
}

/// Clear expired write clogs and wake pending tasks.
pub(super) fn clear_expired_clogs_inner(inner: &mut SimInner) {
    let now = inner.current_time;
    let expired: Vec<ConnectionId> = inner
        .network
        .connection_clogs
        .iter()
        .filter_map(|(id, state)| (now >= state.expires_at).then_some(*id))
        .collect();

    for id in expired {
        inner.network.connection_clogs.remove(&id);
        SimWorld::wake_all(&mut inner.wakers.clog_wakers, id);
    }
}
