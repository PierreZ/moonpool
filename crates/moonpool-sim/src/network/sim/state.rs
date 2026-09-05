//! Mutable state owned by the simulated network engine.
//!
//! # Byte lifecycle
//!
//! Every byte an application writes to a simulated stream moves through four
//! places, always in this order and never skipping one:
//!
//! ```text
//! application write
//!     |  poll_write accepts up to the send buffer's free capacity
//! local send queue        ConnectionState::send_buffer      (sender side)
//!     |  ProcessSendBuffer: latency sampled, chunk put on the wire
//! in flight               ConnectionState::in_flight        (sender side)
//!     |  Delivery event at deliver_at, unless the direction is held
//! peer receive buffer     ConnectionState::receive_buffer   (receiver side)
//!     |  poll_read drains
//! application read
//! ```
//!
//! A fault is defined for every stage. A partition that cuts the direction
//! stalls the send queue *and* freezes what is already in flight
//! ([`ConnectionState::in_flight_held_since`]); a black hole makes in-flight
//! bytes vanish at the instant they would land; an abort discards the send
//! queue and the flight and resets the peer. Bytes that reached the receive
//! buffer are the peer's: nothing on the wire can take them back.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    net::IpAddr,
    time::Duration,
};

use crate::network::NetworkConfiguration;

use super::{ConnectionId, ListenerId};

/// A temporary connection clog.
#[derive(Debug)]
pub(crate) struct ClogState {
    pub(crate) expires_at: Duration,
}

/// Reason for connection closure, distinguishing FIN from RST semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloseReason {
    /// Connection is open.
    #[default]
    None,
    /// Graceful FIN close.
    Graceful,
    /// Aborted RST close.
    Aborted,
}

/// One item that has left its sender and not yet reached the peer's receive
/// buffer: a data chunk or the FIN that ends the stream.
///
/// Items are delivered strictly in `seq` order at `deliver_at`, which only
/// ever moves later (a partition freezes the flight and shifts every item by
/// the time it stayed cut). The `seq` doubles as the identity a scheduled
/// [`NetworkEvent::Delivery`](super::NetworkEvent::Delivery) refers to, so a
/// delivery event that fires for an item already delivered, or re-timed, is a
/// no-op.
#[derive(Debug, Clone)]
pub(crate) struct InFlight {
    pub(crate) seq: u64,
    pub(crate) deliver_at: Duration,
    pub(crate) payload: InFlightPayload,
}

/// What an in-flight item carries.
#[derive(Debug, Clone)]
pub(crate) enum InFlightPayload {
    /// Ordered stream bytes.
    Data(Vec<u8>),
    /// A graceful close; always the last item of a stream.
    Fin,
}

impl InFlightPayload {
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Data(bytes) => bytes.len(),
            Self::Fin => 0,
        }
    }
}

/// A directed partition between two IP addresses.
#[derive(Debug, Clone)]
pub(crate) struct PartitionState {
    pub(crate) expires_at: Duration,
}

/// Bit-packed flags for a simulated connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ConnectionFlags(u16);

impl ConnectionFlags {
    const IS_CLOSED: u16 = 1 << 0;
    const SEND_CLOSED: u16 = 1 << 1;
    const RECV_CLOSED: u16 = 1 << 2;
    const GRACEFUL_CLOSE_PENDING: u16 = 1 << 6;
    const REMOTE_FIN_RECEIVED: u16 = 1 << 7;
    const SEND_IN_PROGRESS: u16 = 1 << 8;
    const SEND_STALLED: u16 = 1 << 9;
    const SEND_BLACK_HOLED: u16 = 1 << 10;

    fn get(self, mask: u16) -> bool {
        (self.0 & mask) != 0
    }

    fn set_bit(&mut self, mask: u16, value: bool) {
        if value {
            self.0 |= mask;
        } else {
            self.0 &= !mask;
        }
    }

    pub(crate) fn is_closed(self) -> bool {
        self.get(Self::IS_CLOSED)
    }

    pub(crate) fn set_is_closed(&mut self, value: bool) {
        self.set_bit(Self::IS_CLOSED, value);
    }

    pub(crate) fn send_closed(self) -> bool {
        self.get(Self::SEND_CLOSED)
    }

    pub(crate) fn set_send_closed(&mut self, value: bool) {
        self.set_bit(Self::SEND_CLOSED, value);
    }

    pub(crate) fn recv_closed(self) -> bool {
        self.get(Self::RECV_CLOSED)
    }

    pub(crate) fn set_recv_closed(&mut self, value: bool) {
        self.set_bit(Self::RECV_CLOSED, value);
    }

    pub(crate) fn graceful_close_pending(self) -> bool {
        self.get(Self::GRACEFUL_CLOSE_PENDING)
    }

    pub(crate) fn set_graceful_close_pending(&mut self, value: bool) {
        self.set_bit(Self::GRACEFUL_CLOSE_PENDING, value);
    }

    pub(crate) fn remote_fin_received(self) -> bool {
        self.get(Self::REMOTE_FIN_RECEIVED)
    }

    pub(crate) fn set_remote_fin_received(&mut self, value: bool) {
        self.set_bit(Self::REMOTE_FIN_RECEIVED, value);
    }

    pub(crate) fn send_in_progress(self) -> bool {
        self.get(Self::SEND_IN_PROGRESS)
    }

    pub(crate) fn set_send_in_progress(&mut self, value: bool) {
        self.set_bit(Self::SEND_IN_PROGRESS, value);
    }

    /// Whether a queued send is held back by a partition.
    ///
    /// A stalled connection owns no scheduled work: it is re-driven when the
    /// partitions blocking it heal.
    pub(crate) fn send_stalled(self) -> bool {
        self.get(Self::SEND_STALLED)
    }

    pub(crate) fn set_send_stalled(&mut self, value: bool) {
        self.set_bit(Self::SEND_STALLED, value);
    }

    /// Whether this endpoint's sends vanish: every chunk drained from the send
    /// buffer is dropped instead of delivered, and its FIN never leaves.
    ///
    /// The flag is permanent for the connection's lifetime; only a fresh
    /// connection is clean again.
    pub(crate) fn send_black_holed(self) -> bool {
        self.get(Self::SEND_BLACK_HOLED)
    }

    pub(crate) fn set_send_black_holed(&mut self, value: bool) {
        self.set_bit(Self::SEND_BLACK_HOLED, value);
    }
}

/// State for one endpoint of a simulated TCP connection.
///
/// The sending half (`send_buffer`, `in_flight`, the delivery clock) belongs
/// to this endpoint and describes the direction `local_ip -> remote_ip`; the
/// receiving half is `receive_buffer`, filled by the peer's deliveries.
#[derive(Debug, Clone)]
pub(crate) struct ConnectionState {
    pub(crate) local_ip: Option<IpAddr>,
    pub(crate) remote_ip: Option<IpAddr>,
    pub(crate) peer_address: String,
    pub(crate) receive_buffer: VecDeque<u8>,
    pub(crate) paired_connection: Option<ConnectionId>,
    /// Chunks accepted from the application and not yet put on the wire.
    pub(crate) send_buffer: VecDeque<Vec<u8>>,
    /// Chunks (and at most one trailing FIN) on the wire towards the peer, in
    /// delivery order.
    pub(crate) in_flight: VecDeque<InFlight>,
    /// Set while a partition freezes the flight: the instant it was cut.
    ///
    /// A held direction delivers nothing. When every partition blocking it has
    /// healed, each in-flight item is re-timed by the time the direction spent
    /// cut, so the cut delays every byte it caught by exactly its own length
    /// and never reorders the stream.
    pub(crate) in_flight_held_since: Option<Duration>,
    /// Next `seq` to hand an in-flight item.
    pub(crate) next_in_flight_seq: u64,
    /// Delivery time of the most recent item put on the wire; the next item
    /// is delivered strictly after it, which is what keeps the stream FIFO.
    pub(crate) last_delivery_at: Option<Duration>,
    pub(crate) flags: ConnectionFlags,
    pub(crate) close_reason: CloseReason,
    pub(crate) send_buffer_capacity: usize,
}

impl ConnectionState {
    /// Bytes waiting in the local send queue.
    pub(crate) fn queued_bytes(&self) -> usize {
        self.send_buffer.iter().map(Vec::len).sum()
    }

    /// Bytes on the wire (excluding the FIN, which carries none).
    pub(crate) fn in_flight_bytes(&self) -> usize {
        self.in_flight.iter().map(|item| item.payload.len()).sum()
    }
}

/// Protocol state owned exclusively by the simulated network engine.
#[derive(Debug)]
pub(crate) struct NetworkState {
    pub(crate) next_connection_id: u64,
    pub(crate) next_listener_id: u64,
    pub(crate) config: NetworkConfiguration,
    pub(crate) connections: BTreeMap<ConnectionId, ConnectionState>,
    pub(crate) listeners: BTreeSet<ListenerId>,
    pub(crate) pending_connections: BTreeMap<String, VecDeque<ConnectionId>>,
    pub(crate) connection_clogs: BTreeMap<ConnectionId, ClogState>,
    pub(crate) read_clogs: BTreeMap<ConnectionId, ClogState>,
    pub(crate) ip_partitions: BTreeMap<(IpAddr, IpAddr), PartitionState>,
    pub(crate) send_partitions: BTreeMap<IpAddr, Duration>,
    pub(crate) recv_partitions: BTreeMap<IpAddr, Duration>,
    pub(crate) last_random_close_time: Duration,
    pub(crate) last_black_hole_time: Duration,
    pub(crate) pair_latencies: BTreeMap<(IpAddr, IpAddr), Duration>,
}

impl NetworkState {
    pub(crate) fn new(config: NetworkConfiguration) -> Self {
        Self {
            next_connection_id: 0,
            next_listener_id: 0,
            config,
            connections: BTreeMap::new(),
            listeners: BTreeSet::new(),
            pending_connections: BTreeMap::new(),
            connection_clogs: BTreeMap::new(),
            read_clogs: BTreeMap::new(),
            ip_partitions: BTreeMap::new(),
            send_partitions: BTreeMap::new(),
            recv_partitions: BTreeMap::new(),
            last_random_close_time: Duration::ZERO,
            last_black_hole_time: Duration::ZERO,
            pair_latencies: BTreeMap::new(),
        }
    }

    pub(crate) fn parse_ip_from_addr(addr: &str) -> Option<IpAddr> {
        if addr.starts_with('[')
            && let Some(bracket_end) = addr.find(']')
        {
            return addr[1..bracket_end].parse().ok();
        }
        if let Some(colon_pos) = addr.rfind(':') {
            addr[..colon_pos].parse().ok()
        } else {
            addr.parse().ok()
        }
    }

    /// Deadline at which every partition blocking `from_ip -> to_ip` has healed.
    ///
    /// Returns `None` when the direction is not partitioned at `now`. Directed,
    /// send-side, and receive-side partitions can overlap, so the caller has to
    /// wait for the latest of the deadlines currently in force.
    pub(crate) fn partition_clear_at(
        &self,
        from_ip: IpAddr,
        to_ip: IpAddr,
        now: Duration,
    ) -> Option<Duration> {
        [
            self.ip_partitions
                .get(&(from_ip, to_ip))
                .map(|partition| partition.expires_at),
            self.send_partitions.get(&from_ip).copied(),
            self.recv_partitions.get(&to_ip).copied(),
        ]
        .into_iter()
        .flatten()
        .filter(|expires_at| now < *expires_at)
        .max()
    }

    pub(crate) fn is_partitioned(&self, from_ip: IpAddr, to_ip: IpAddr, now: Duration) -> bool {
        self.partition_clear_at(from_ip, to_ip, now).is_some()
    }

    /// Deadline at which `connection_id` may send again, if it is partitioned.
    pub(crate) fn connection_partition_clear_at(
        &self,
        connection_id: ConnectionId,
        now: Duration,
    ) -> Option<Duration> {
        let connection = self.connections.get(&connection_id)?;
        self.partition_clear_at(connection.local_ip?, connection.remote_ip?, now)
    }
}
