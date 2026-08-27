//! Mutable state owned by the simulated network engine.

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
    const IS_STABLE: u16 = 1 << 5;
    const GRACEFUL_CLOSE_PENDING: u16 = 1 << 6;
    const REMOTE_FIN_RECEIVED: u16 = 1 << 7;
    const SEND_IN_PROGRESS: u16 = 1 << 8;
    const SEND_STALLED: u16 = 1 << 9;

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

    pub(crate) fn is_stable(self) -> bool {
        self.get(Self::IS_STABLE)
    }

    pub(crate) fn set_is_stable(&mut self, value: bool) {
        self.set_bit(Self::IS_STABLE, value);
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
}

/// State for one endpoint of a simulated TCP connection.
#[derive(Debug, Clone)]
pub(crate) struct ConnectionState {
    pub(crate) local_ip: Option<IpAddr>,
    pub(crate) remote_ip: Option<IpAddr>,
    pub(crate) peer_address: String,
    pub(crate) receive_buffer: VecDeque<u8>,
    pub(crate) paired_connection: Option<ConnectionId>,
    pub(crate) send_buffer: VecDeque<Vec<u8>>,
    pub(crate) next_send_time: Duration,
    pub(crate) flags: ConnectionFlags,
    pub(crate) close_reason: CloseReason,
    pub(crate) send_buffer_capacity: usize,
    pub(crate) send_delay: Option<Duration>,
    pub(crate) last_data_delivery_scheduled_at: Option<Duration>,
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
