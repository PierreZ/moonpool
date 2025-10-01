use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    time::Duration,
};

use crate::network::{
    NetworkConfiguration,
    sim::{ConnectionId, ListenerId},
};

/// Simple clog state - just tracks when it expires
#[derive(Debug)]
pub struct ClogState {
    /// When the clog expires and writes can resume (in simulation time)
    pub expires_at: Duration,
}

/// Network partition state between two IP addresses
#[derive(Debug, Clone)]
pub struct PartitionState {
    /// When the partition expires and connectivity is restored (in simulation time)
    pub expires_at: Duration,
}

/// Internal connection state for simulation
#[derive(Debug, Clone)]
pub struct ConnectionState {
    /// Unique identifier for this connection within the simulation.
    #[allow(dead_code)]
    pub id: ConnectionId,

    /// Network address this connection is associated with.
    #[allow(dead_code)]
    pub addr: String,

    /// Local IP address for this connection
    pub local_ip: Option<IpAddr>,

    /// Remote IP address for this connection
    pub remote_ip: Option<IpAddr>,

    /// FIFO buffer for incoming data that hasn't been read by the application yet.
    pub receive_buffer: VecDeque<u8>,

    /// Reference to the other end of this bidirectional TCP connection.
    pub paired_connection: Option<ConnectionId>,

    /// FIFO buffer for outgoing data waiting to be sent over the network.
    pub send_buffer: VecDeque<Vec<u8>>,

    /// Flag indicating whether a `ProcessSendBuffer` event is currently scheduled.
    pub send_in_progress: bool,

    /// Next available time for sending messages from this connection.
    pub next_send_time: Duration,

    /// Whether this connection has been permanently closed by one of the endpoints
    pub is_closed: bool,
}

/// Internal listener state for simulation
#[derive(Debug)]
pub struct ListenerState {
    #[allow(dead_code)]
    pub id: ListenerId,
    #[allow(dead_code)]
    pub addr: String,
    #[allow(dead_code)]
    pub pending_connections: VecDeque<ConnectionId>,
}

/// Network-related state management
#[derive(Debug)]
pub struct NetworkState {
    pub next_connection_id: u64,
    pub next_listener_id: u64,
    pub config: NetworkConfiguration,
    pub connections: HashMap<ConnectionId, ConnectionState>,
    pub listeners: HashMap<ListenerId, ListenerState>,
    pub pending_connections: HashMap<String, ConnectionId>,

    // Connection disruption state
    pub connection_clogs: HashMap<ConnectionId, ClogState>,

    // Network partition state
    /// Partitions between specific IP pairs (from, to) -> partition state
    pub ip_partitions: HashMap<(IpAddr, IpAddr), PartitionState>,
    /// Send partitions - IP cannot send to anyone
    pub send_partitions: HashMap<IpAddr, Duration>,
    /// Receive partitions - IP cannot receive from anyone
    pub recv_partitions: HashMap<IpAddr, Duration>,
}

impl NetworkState {
    pub fn new(config: NetworkConfiguration) -> Self {
        Self {
            next_connection_id: 0,
            next_listener_id: 0,
            config,
            connections: HashMap::new(),
            listeners: HashMap::new(),
            pending_connections: HashMap::new(),
            connection_clogs: HashMap::new(),
            ip_partitions: HashMap::new(),
            send_partitions: HashMap::new(),
            recv_partitions: HashMap::new(),
        }
    }

    /// Extract IP address from a network address string
    /// Supports formats like "127.0.0.1:8080", "[::1]:8080", etc.
    pub fn parse_ip_from_addr(addr: &str) -> Option<IpAddr> {
        // Handle IPv6 addresses in brackets
        if addr.starts_with('[')
            && let Some(bracket_end) = addr.find(']')
        {
            return addr[1..bracket_end].parse().ok();
        }

        // Handle IPv4 addresses and unbracketed IPv6
        if let Some(colon_pos) = addr.rfind(':') {
            addr[..colon_pos].parse().ok()
        } else {
            addr.parse().ok()
        }
    }

    /// Check if communication from source IP to destination IP is partitioned
    pub fn is_partitioned(&self, from_ip: IpAddr, to_ip: IpAddr, current_time: Duration) -> bool {
        // Check IP pair partition
        if let Some(partition) = self.ip_partitions.get(&(from_ip, to_ip))
            && current_time < partition.expires_at
        {
            return true;
        }

        // Check send partition
        if let Some(&partition_until) = self.send_partitions.get(&from_ip)
            && current_time < partition_until
        {
            return true;
        }

        // Check receive partition
        if let Some(&partition_until) = self.recv_partitions.get(&to_ip)
            && current_time < partition_until
        {
            return true;
        }

        false
    }

    /// Check if a connection is partitioned (cannot send messages)
    pub fn is_connection_partitioned(
        &self,
        connection_id: ConnectionId,
        current_time: Duration,
    ) -> bool {
        if let Some(conn) = self.connections.get(&connection_id)
            && let (Some(local_ip), Some(remote_ip)) = (conn.local_ip, conn.remote_ip)
        {
            return self.is_partitioned(local_ip, remote_ip, current_time);
        }
        false
    }
}
