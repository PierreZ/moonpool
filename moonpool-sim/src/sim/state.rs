//! Network state management for simulation.
//!
//! This module provides internal state types for managing connections,
//! listeners, partitions, and clogs in the simulation environment.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::IpAddr,
    time::Duration,
};

use moonpool_core::OpenOptions;

use crate::{
    network::{
        NetworkConfiguration,
        sim::{ConnectionId, ListenerId},
    },
    storage::{InMemoryStorage, StorageConfiguration},
};

/// Simple clog state - just tracks when it expires
#[derive(Debug)]
pub struct ClogState {
    /// When the clog expires and writes can resume (in simulation time)
    pub expires_at: Duration,
}

/// Reason for connection closure - distinguishes FIN vs RST semantics.
///
/// In real TCP:
/// - FIN (graceful close): Peer gets EOF on read, writes may still work briefly
/// - RST (aborted close): Peer gets ECONNRESET immediately on both read and write
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloseReason {
    /// Connection is not closed
    #[default]
    None,
    /// Graceful close (FIN) - peer will get EOF on read
    Graceful,
    /// Aborted close (RST) - peer will get ECONNRESET
    Aborted,
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

    /// Peer address as seen by this connection.
    ///
    /// FDB Pattern (sim2.actor.cpp:1149-1175):
    /// - For client-side connections: the destination address (server's listening address)
    /// - For server-side connections: synthesized ephemeral address (random IP + port 40000-60000)
    ///
    /// This simulates real TCP behavior where servers see client ephemeral ports,
    /// not the client's identity address. As FDB notes: "In the case of an incoming
    /// connection, this may not be an address we can connect to!"
    pub peer_address: String,

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

    /// Whether the send side is closed (writes will fail) - for asymmetric closure
    /// FDB: closeInternal() on self closes send capability
    pub send_closed: bool,

    /// Whether the receive side is closed (reads return EOF) - for asymmetric closure
    /// FDB: closeInternal() on peer closes recv capability
    pub recv_closed: bool,

    /// Whether this connection is temporarily cut (will be restored).
    /// Unlike `is_closed`, a cut connection can be restored after a duration.
    /// This simulates temporary network outages where the connection is not
    /// permanently severed but temporarily unavailable.
    pub is_cut: bool,

    /// When the cut expires and the connection is restored (in simulation time).
    /// Only meaningful when `is_cut` is true.
    pub cut_expiry: Option<Duration>,

    /// Reason for connection closure - distinguishes FIN vs RST semantics.
    /// When `is_closed` is true, this indicates whether it was graceful or aborted.
    pub close_reason: CloseReason,

    /// Send buffer capacity in bytes.
    /// When the send buffer reaches this limit, poll_write returns Pending.
    /// Calculated from BDP (Bandwidth-Delay Product): latency × bandwidth.
    pub send_buffer_capacity: usize,

    /// Per-connection send delay override (asymmetric latency).
    /// If set, this delay is applied when sending data from this connection.
    /// If None, the global write_latency from NetworkConfiguration is used.
    pub send_delay: Option<Duration>,

    /// Per-connection receive delay override (asymmetric latency).
    /// If set, this delay is applied when receiving data on this connection.
    /// If None, the global read_latency from NetworkConfiguration is used.
    pub recv_delay: Option<Duration>,

    /// Whether this connection is in a half-open state (peer crashed).
    /// In this state:
    /// - Local side still thinks it's connected
    /// - Writes succeed but data is silently discarded
    /// - Reads block waiting for data that will never come
    /// - After `half_open_error_at`, errors start manifesting
    pub is_half_open: bool,

    /// When a half-open connection starts returning errors (in simulation time).
    /// Before this time: writes succeed (data dropped), reads block.
    /// After this time: both read and write return ECONNRESET.
    pub half_open_error_at: Option<Duration>,

    /// Whether this connection is stable (exempt from chaos).
    ///
    /// FDB ref: sim2.actor.cpp:357-362, 427, 440, 581-582 (stableConnection flag)
    ///
    /// Stable connections are exempt from:
    /// - Random close (`roll_random_close`)
    /// - Write clogging
    /// - Read clogging
    /// - Bit flip corruption
    /// - Partial write truncation
    ///
    /// This is used for parent-child process connections or supervision channels
    /// that should remain reliable even during chaos testing.
    pub is_stable: bool,
}

/// Internal listener state for simulation
#[derive(Debug)]
pub struct ListenerState {
    /// Unique identifier for this listener.
    #[allow(dead_code)]
    pub id: ListenerId,
    /// Network address this listener is bound to.
    #[allow(dead_code)]
    pub addr: String,
    /// Queue of pending connections waiting to be accepted.
    #[allow(dead_code)]
    pub pending_connections: VecDeque<ConnectionId>,
}

/// Network-related state management
#[derive(Debug)]
pub struct NetworkState {
    /// Counter for generating unique connection IDs.
    pub next_connection_id: u64,
    /// Counter for generating unique listener IDs.
    pub next_listener_id: u64,
    /// Network configuration for this simulation.
    pub config: NetworkConfiguration,
    /// Active connections indexed by their ID.
    pub connections: HashMap<ConnectionId, ConnectionState>,
    /// Active listeners indexed by their ID.
    pub listeners: HashMap<ListenerId, ListenerState>,
    /// Connections pending acceptance, indexed by address.
    pub pending_connections: HashMap<String, ConnectionId>,

    /// Write clog state (temporary write blocking).
    pub connection_clogs: HashMap<ConnectionId, ClogState>,

    /// Read clog state (temporary read blocking, symmetric with write clogging).
    pub read_clogs: HashMap<ConnectionId, ClogState>,

    /// Partitions between specific IP pairs (from, to) -> partition state
    pub ip_partitions: HashMap<(IpAddr, IpAddr), PartitionState>,
    /// Send partitions - IP cannot send to anyone
    pub send_partitions: HashMap<IpAddr, Duration>,
    /// Receive partitions - IP cannot receive from anyone
    pub recv_partitions: HashMap<IpAddr, Duration>,

    /// Last time a random close was triggered (global cooldown tracking)
    /// FDB: g_simulator->lastConnectionFailure - see sim2.actor.cpp:583
    pub last_random_close_time: Duration,

    /// Per-IP-pair base latencies for consistent connection behavior.
    /// Once set on first connection, all subsequent connections between the same
    /// IP pair will use this base latency (with optional jitter on top).
    pub pair_latencies: HashMap<(IpAddr, IpAddr), Duration>,
}

impl NetworkState {
    /// Create a new network state with the given configuration.
    pub fn new(config: NetworkConfiguration) -> Self {
        Self {
            next_connection_id: 0,
            next_listener_id: 0,
            config,
            connections: HashMap::new(),
            listeners: HashMap::new(),
            pending_connections: HashMap::new(),
            connection_clogs: HashMap::new(),
            read_clogs: HashMap::new(),
            ip_partitions: HashMap::new(),
            send_partitions: HashMap::new(),
            recv_partitions: HashMap::new(),
            last_random_close_time: Duration::ZERO,
            pair_latencies: HashMap::new(),
        }
    }

    /// Extract IP address from a network address string.
    /// Supports formats like "127.0.0.1:8080", "\[::1\]:8080", etc.
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

// =============================================================================
// Storage State Types
// =============================================================================

/// Unique identifier for a simulated file within the simulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(pub u64);

/// Type of pending storage operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOpType {
    /// Read operation
    Read,
    /// Write operation
    Write,
    /// Sync/flush operation
    Sync,
    /// Set file length operation
    SetLen,
    /// File open operation
    Open,
}

/// A pending storage operation awaiting completion.
#[derive(Debug, Clone)]
pub struct PendingStorageOp {
    /// Type of the operation
    pub op_type: PendingOpType,
    /// Offset within the file (for read/write)
    pub offset: u64,
    /// Length of the operation in bytes
    pub len: usize,
    /// Data for write operations
    pub data: Option<Vec<u8>>,
}

/// State of an individual simulated file.
#[derive(Debug)]
pub struct StorageFileState {
    /// Unique identifier for this file
    pub id: FileId,
    /// Path this file was opened with
    pub path: String,
    /// Current file position for sequential operations
    pub position: u64,
    /// Options the file was opened with
    pub options: OpenOptions,
    /// In-memory storage backing this file
    pub storage: InMemoryStorage,
    /// Whether the file has been closed
    pub is_closed: bool,
    /// Pending operations keyed by sequence number
    pub pending_ops: HashMap<u64, PendingStorageOp>,
    /// Next sequence number for operations on this file
    pub next_op_seq: u64,
}

impl StorageFileState {
    /// Create a new storage file state.
    pub fn new(id: FileId, path: String, options: OpenOptions, storage: InMemoryStorage) -> Self {
        Self {
            id,
            path,
            position: 0,
            options,
            storage,
            is_closed: false,
            pending_ops: HashMap::new(),
            next_op_seq: 0,
        }
    }
}

/// Storage-related state management for the simulation.
#[derive(Debug)]
pub struct StorageState {
    /// Counter for generating unique file IDs
    pub next_file_id: u64,
    /// Storage configuration for latencies and fault injection
    pub config: StorageConfiguration,
    /// Active files indexed by their ID
    pub files: HashMap<FileId, StorageFileState>,
    /// Mapping from path to file ID for quick lookup
    pub path_to_file: HashMap<String, FileId>,
    /// Set of paths that have been deleted (for create_new semantics)
    pub deleted_paths: HashSet<String>,
}

impl StorageState {
    /// Create a new storage state with the given configuration.
    pub fn new(config: StorageConfiguration) -> Self {
        Self {
            next_file_id: 0,
            config,
            files: HashMap::new(),
            path_to_file: HashMap::new(),
            deleted_paths: HashSet::new(),
        }
    }
}

impl Default for StorageState {
    fn default() -> Self {
        Self::new(StorageConfiguration::default())
    }
}
