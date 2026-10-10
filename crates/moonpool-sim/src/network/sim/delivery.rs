//! Data delivery: the send paths, in-flight hold and release, landing, corruption and clogging.

use std::{
    collections::{BTreeMap, BTreeSet},
    task::Waker,
    time::Duration,
};

use crate::{
    chaos::fault_events::SimFaultEvent,
    sim::{
        rng::{sim_random, sim_random_range},
        wakers::WakeBatch,
    },
};

use super::engine::{NetworkActions, NetworkSimulation};
use super::{
    ConnectionId, NetworkEvent,
    state::{ClogState, InFlight, InFlightPayload},
};

impl NetworkSimulation {
    /// Land every item at the head of `sender`'s flight whose delivery time
    /// has come.
    ///
    /// The event that wakes this names one item, but the flight is the truth:
    /// an item is delivered only from the head, only once its `deliver_at`
    /// has passed, and never while the direction is held by a partition. A
    /// stale event (its item already delivered, or re-timed by a heal) finds
    /// nothing to do. Faults are judged *now*, at landing, not when the
    /// chunk was put on the wire: a partition or black hole injected while
    /// the bytes were in flight still decides their fate.
    pub(super) fn handle_delivery(
        &mut self,
        sender: ConnectionId,
        seq: u64,
        now: Duration,
        actions: &mut NetworkActions,
        wakes: &mut WakeBatch,
    ) {
        loop {
            let Some(connection) = self.state.connections.get(&sender) else {
                return;
            };
            if connection.in_flight_held_since.is_some() {
                return;
            }
            let Some(head) = connection.in_flight.front() else {
                return;
            };
            if head.deliver_at > now {
                return;
            }
            // A partition that somehow reached this direction without freezing
            // the flight (there is no such path today) still holds it here,
            // from this instant, rather than letting the bytes cross the cut.
            if self
                .state
                .connection_partition_clear_at(sender, now)
                .is_some()
            {
                self.hold_in_flight(sender, now);
                return;
            }
            let Some(connection) = self.state.connections.get_mut(&sender) else {
                return;
            };
            let Some(item) = connection.in_flight.pop_front() else {
                return;
            };
            debug_assert!(
                item.seq <= seq,
                "a delivery event never lands an item queued after the one it names"
            );
            self.land(sender, item.payload, now, actions, wakes);
        }
    }

    /// Put one item that reached its delivery time where it belongs.
    fn land(
        &mut self,
        sender: ConnectionId,
        payload: InFlightPayload,
        now: Duration,
        actions: &mut NetworkActions,
        wakes: &mut WakeBatch,
    ) {
        let Some((black_holed, receiver)) = self.state.connections.get(&sender).map(|connection| {
            (
                connection.flags.send_black_holed(),
                connection.paired_connection,
            )
        }) else {
            return;
        };
        // The item left a black-holed sender: it was acknowledged into that
        // side's window and is gone. No bytes, no EOF, no wake, and the
        // window credit it took is never returned — the reader keeps waiting
        // for data that will never come, and once the window is full the
        // writer waits with it.
        if black_holed {
            return;
        }
        let receiver_open = receiver
            .and_then(|id| self.state.connections.get(&id))
            .is_some_and(|connection| !connection.flags.is_closed());
        match payload {
            InFlightPayload::Data(data) => {
                let receiver_reading = receiver
                    .and_then(|id| self.state.connections.get(&id))
                    .is_some_and(|connection| {
                        !connection.flags.is_closed() && !connection.flags.recv_closed()
                    });
                let Some(receiver) = receiver.filter(|_| receiver_reading) else {
                    // A closed or receive-shut peer discards what arrives and
                    // acknowledges it, so the writer is not left waiting on
                    // credits nobody will ever read back.
                    self.release_window(sender, data.len(), wakes);
                    return;
                };
                let delivered = self.maybe_corrupt_data(receiver, &data, now, actions);
                if let Some(connection) = self.state.connections.get_mut(&receiver) {
                    connection.receive_buffer.extend(delivered);
                }
                wakes.push(self.waiters.reads.take(&receiver));
            }
            InFlightPayload::Fin => {
                if let Some(receiver) = receiver.filter(|_| receiver_open)
                    && let Some(connection) = self.state.connections.get_mut(&receiver)
                {
                    connection.flags.set_remote_fin_received(true);
                    wakes.push(self.waiters.reads.take(&receiver));
                }
            }
        }
    }

    /// Put `payload` on the wire from `id`, strictly after everything already
    /// in flight, and schedule the event that will land it.
    ///
    /// `at` is the delivery time the sender computed; the flight's FIFO floor
    /// (`last_delivery_at + 1ns`) still applies. Enqueuing into a direction a
    /// partition currently cuts freezes the flight from this instant, which
    /// is how a FIN queued under a partition (the one send that bypasses the
    /// send queue) waits for the heal like everything else.
    fn put_in_flight(
        &mut self,
        id: ConnectionId,
        payload: InFlightPayload,
        at: Duration,
        now: Duration,
        actions: &mut NetworkActions,
    ) {
        let partitioned = self.state.connection_partition_clear_at(id, now).is_some();
        let Some(connection) = self.state.connections.get_mut(&id) else {
            return;
        };
        let deliver_at = connection.last_delivery_at.map_or(at, |last| {
            at.max(last.saturating_add(Duration::from_nanos(1)))
        });
        let seq = connection.next_in_flight_seq;
        connection.next_in_flight_seq += 1;
        connection.last_delivery_at = Some(deliver_at);
        connection.in_flight.push_back(InFlight {
            seq,
            deliver_at,
            payload,
        });
        if partitioned && connection.in_flight_held_since.is_none() {
            connection.in_flight_held_since = Some(now);
        }
        actions.schedule_at(
            deliver_at,
            NetworkEvent::Delivery {
                connection_id: id,
                seq,
            },
        );
    }

    /// Freeze `id`'s flight as of `now`. Idempotent while held.
    fn hold_in_flight(&mut self, id: ConnectionId, now: Duration) {
        if let Some(connection) = self.state.connections.get_mut(&id)
            && connection.in_flight_held_since.is_none()
            && !connection.in_flight.is_empty()
        {
            connection.in_flight_held_since = Some(now);
        }
    }

    /// A partition just went up: freeze the flight of every direction it
    /// cuts. Bytes on the wire when a cut lands do not cross it.
    pub(super) fn hold_partitioned_in_flight(&mut self, now: Duration) {
        let cut = self
            .state
            .connections
            .iter()
            .filter(|(id, connection)| {
                connection.in_flight_held_since.is_none()
                    && !connection.in_flight.is_empty()
                    && self
                        .state
                        .connection_partition_clear_at(**id, now)
                        .is_some()
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in cut {
            self.hold_in_flight(id, now);
        }
    }

    /// Thaw every held flight whose partitions have all healed.
    ///
    /// Each item is re-timed by the time the direction spent cut and its
    /// delivery event re-scheduled; the FIFO floor moves with it, so a chunk
    /// sent after the heal still lands behind the last one that was frozen.
    fn release_held_in_flight(&mut self, now: Duration, actions: &mut NetworkActions) {
        let thawed = self
            .state
            .connections
            .iter()
            .filter_map(|(id, connection)| {
                connection.in_flight_held_since.and_then(|since| {
                    self.state
                        .connection_partition_clear_at(*id, now)
                        .is_none()
                        .then_some((*id, since))
                })
            })
            .collect::<Vec<_>>();
        for (id, since) in thawed {
            let Some(connection) = self.state.connections.get_mut(&id) else {
                continue;
            };
            let shift = now.saturating_sub(since);
            connection.in_flight_held_since = None;
            connection.last_delivery_at = connection
                .last_delivery_at
                .map(|last| last.saturating_add(shift));
            for item in &mut connection.in_flight {
                item.deliver_at = item.deliver_at.saturating_add(shift);
                actions.schedule_at(
                    item.deliver_at,
                    NetworkEvent::Delivery {
                        connection_id: id,
                        seq: item.seq,
                    },
                );
            }
        }
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

    pub(super) fn handle_process_send_buffer(
        &mut self,
        id: ConnectionId,
        now: Duration,
        actions: &mut NetworkActions,
        wakes: &mut WakeBatch,
    ) {
        if self.connection(id).is_none_or(|connection| {
            (connection.flags.is_closed() || connection.flags.send_closed())
                && !connection.flags.graceful_close_pending()
        }) {
            self.discard_send_queue(id);
            if let Some(connection) = self.state.connections.get_mut(&id) {
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
            .connection(id)
            .is_some_and(|connection| !connection.send_buffer.is_empty());
        if has_queued_bytes && self.state.connection_partition_clear_at(id, now).is_some() {
            self.stall_partitioned_send(id);
        } else {
            self.handle_normal_send(id, now, actions);
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
    /// Send-window waiters are deliberately left registered: no window is
    /// released while the stream is stalled (nothing reaches the peer's
    /// reader), so writers keep seeing backpressure until the bytes land and
    /// are read.
    ///
    /// A stalled connection owns no scheduled work. It is re-driven by
    /// [`resume_after_heal`](Self::resume_after_heal) when the partitions
    /// blocking it heal, whether that happens at their deadline or earlier.
    fn stall_partitioned_send(&mut self, id: ConnectionId) {
        if let Some(connection) = self.state.connections.get_mut(&id) {
            connection.flags.set_send_stalled(true);
        }
    }

    /// Re-drive every direction whose blocking partitions have healed: thaw
    /// what was frozen in flight, then resume the stalled send queue behind
    /// it.
    ///
    /// Runs from each partition-clearing path, so a stream stalled by a
    /// partition that is healed early releases its bytes early instead of
    /// waiting out the deadline it stalled under. Consumes no randomness: the
    /// thawed items keep the latency they sampled, and the queued chunk
    /// samples its own when its `ProcessSendBuffer` runs, as it always did.
    pub(super) fn resume_after_heal(&mut self, now: Duration, actions: &mut NetworkActions) {
        self.release_held_in_flight(now, actions);
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
    ) {
        let Some(snapshot) = self.connection(id).map(|connection| {
            (
                connection.paired_connection,
                connection.local_ip,
                connection.remote_ip,
            )
        }) else {
            return;
        };
        let (paired_id, local_ip, remote_ip) = snapshot;
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
                self.put_fin_in_flight(id, now, actions);
            }
            return;
        };
        // No window is released here: the bytes have only moved from the
        // queue onto the wire, and they stay charged to the writer until the
        // peer's application reads them.
        if crate::buggify!() && !data.is_empty() {
            let max_send = data.len().min(partial_max);
            // At least one byte goes out: an empty chunk would consume a
            // sequence number and a delivery event and wake the reader for
            // nothing.
            let truncate_to = sim_random_range(1..max_send + 1);
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
        let at = now.saturating_add(base_delay).saturating_add(pair_extra);
        let queue_drained = connection.send_buffer.is_empty();
        let close_pending = connection.flags.graceful_close_pending();
        if paired_id.is_some() {
            self.put_in_flight(id, InFlightPayload::Data(data), at, now, actions);
        }
        let Some(connection) = self.state.connections.get_mut(&id) else {
            return;
        };
        if queue_drained {
            connection.flags.set_send_in_progress(false);
            if close_pending {
                connection.flags.set_graceful_close_pending(false);
                self.put_fin_in_flight(id, now, actions);
            }
        } else {
            actions.schedule_at(now, NetworkEvent::ProcessSendBuffer { connection_id: id });
        }
    }

    /// Put `id`'s FIN on the wire, behind every data chunk in flight.
    pub(super) fn put_fin_in_flight(
        &mut self,
        id: ConnectionId,
        now: Duration,
        actions: &mut NetworkActions,
    ) {
        if self
            .connection(id)
            .is_none_or(|connection| connection.paired_connection.is_none())
        {
            return;
        }
        self.put_in_flight(
            id,
            InFlightPayload::Fin,
            now.saturating_add(Duration::from_nanos(1)),
            now,
            actions,
        );
    }

    /// Whether a clog tracked in `clogs` is in force for `id`, or, when none
    /// is tracked, whether the clog coin fires now.
    fn should_clog(
        &self,
        clogs: &BTreeMap<ConnectionId, ClogState>,
        id: ConnectionId,
        now: Duration,
    ) -> bool {
        if let Some(clog) = clogs.get(&id) {
            return now < clog.expires_at;
        }
        let probability = self.state.config.chaos.clog_probability;
        probability > 0.0 && sim_random::<f64>() < probability
    }

    /// Sample a clog duration and return the deadline at which a clog
    /// starting `now` clears.
    fn sample_clog_deadline(&self, now: Duration) -> Duration {
        let duration = crate::network::sample_duration(&self.state.config.chaos.clog_duration);
        now.saturating_add(duration)
    }

    fn is_clogged(
        clogs: &BTreeMap<ConnectionId, ClogState>,
        id: ConnectionId,
        now: Duration,
    ) -> bool {
        clogs.get(&id).is_some_and(|c| now < c.expires_at)
    }

    pub(crate) fn should_clog_write(&self, id: ConnectionId, now: Duration) -> bool {
        self.should_clog(&self.state.connection_clogs, id, now)
    }

    pub(crate) fn clog_write(&mut self, id: ConnectionId, now: Duration) -> NetworkActions {
        let deadline = self.sample_clog_deadline(now);
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
        Self::is_clogged(&self.state.connection_clogs, id, now)
    }

    pub(crate) fn register_write_clog(&mut self, id: ConnectionId, waker: &Waker) -> bool {
        if self
            .connection(id)
            .is_none_or(|connection| connection.flags.is_closed())
        {
            return false;
        }
        self.waiters.write_clogs.register(id, waker);
        true
    }

    pub(crate) fn should_clog_read(&self, id: ConnectionId, now: Duration) -> bool {
        self.should_clog(&self.state.read_clogs, id, now)
    }

    pub(crate) fn clog_read(&mut self, id: ConnectionId, now: Duration) -> NetworkActions {
        let deadline = self.sample_clog_deadline(now);
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
        Self::is_clogged(&self.state.read_clogs, id, now)
    }

    pub(crate) fn register_read_clog(&mut self, id: ConnectionId, waker: &Waker) -> bool {
        if self
            .connection(id)
            .is_none_or(|connection| connection.flags.is_closed())
        {
            return false;
        }
        self.waiters.read_clogs.register(id, waker);
        true
    }
}
