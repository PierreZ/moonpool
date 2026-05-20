//! Network state management for simulation.
//!
//! This module provides internal state types for managing connections,
//! listeners, partitions, and clogs in the simulation environment.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
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

/// Bit-packed boolean flags for a [`ConnectionState`].
///
/// Storing the various closure / chaos flags in a single integer avoids the
/// `clippy::struct_excessive_bools` lint while keeping a small, copy-friendly
/// representation. Helper methods provide named access to each flag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnectionFlags(u16);

impl ConnectionFlags {
    const IS_CLOSED: u16 = 1 << 0;
    const SEND_CLOSED: u16 = 1 << 1;
    const RECV_CLOSED: u16 = 1 << 2;
    const IS_CUT: u16 = 1 << 3;
    const IS_HALF_OPEN: u16 = 1 << 4;
    const IS_STABLE: u16 = 1 << 5;
    const GRACEFUL_CLOSE_PENDING: u16 = 1 << 6;
    const REMOTE_FIN_RECEIVED: u16 = 1 << 7;
    const SEND_IN_PROGRESS: u16 = 1 << 8;

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

    /// Whether the connection has been permanently closed.
    #[must_use]
    pub fn is_closed(self) -> bool {
        self.get(Self::IS_CLOSED)
    }
    /// Update the closed flag.
    pub fn set_is_closed(&mut self, value: bool) {
        self.set_bit(Self::IS_CLOSED, value);
    }

    /// Whether the send side has been closed.
    #[must_use]
    pub fn send_closed(self) -> bool {
        self.get(Self::SEND_CLOSED)
    }
    /// Update the send-closed flag.
    pub fn set_send_closed(&mut self, value: bool) {
        self.set_bit(Self::SEND_CLOSED, value);
    }

    /// Whether the receive side has been closed.
    #[must_use]
    pub fn recv_closed(self) -> bool {
        self.get(Self::RECV_CLOSED)
    }
    /// Update the recv-closed flag.
    pub fn set_recv_closed(&mut self, value: bool) {
        self.set_bit(Self::RECV_CLOSED, value);
    }

    /// Whether the connection is temporarily cut.
    #[must_use]
    pub fn is_cut(self) -> bool {
        self.get(Self::IS_CUT)
    }
    /// Update the cut flag.
    pub fn set_is_cut(&mut self, value: bool) {
        self.set_bit(Self::IS_CUT, value);
    }

    /// Whether the connection is half-open (peer crashed).
    #[must_use]
    pub fn is_half_open(self) -> bool {
        self.get(Self::IS_HALF_OPEN)
    }
    /// Update the half-open flag.
    pub fn set_is_half_open(&mut self, value: bool) {
        self.set_bit(Self::IS_HALF_OPEN, value);
    }

    /// Whether the connection is exempt from chaos (stable).
    #[must_use]
    pub fn is_stable(self) -> bool {
        self.get(Self::IS_STABLE)
    }
    /// Update the stable flag.
    pub fn set_is_stable(&mut self, value: bool) {
        self.set_bit(Self::IS_STABLE, value);
    }

    /// Whether a graceful close (FIN) is pending delivery to the peer.
    #[must_use]
    pub fn graceful_close_pending(self) -> bool {
        self.get(Self::GRACEFUL_CLOSE_PENDING)
    }
    /// Update the graceful-close-pending flag.
    pub fn set_graceful_close_pending(&mut self, value: bool) {
        self.set_bit(Self::GRACEFUL_CLOSE_PENDING, value);
    }

    /// Whether the peer's FIN has been received on this side.
    #[must_use]
    pub fn remote_fin_received(self) -> bool {
        self.get(Self::REMOTE_FIN_RECEIVED)
    }
    /// Update the remote-fin-received flag.
    pub fn set_remote_fin_received(&mut self, value: bool) {
        self.set_bit(Self::REMOTE_FIN_RECEIVED, value);
    }

    /// Whether a `ProcessSendBuffer` event is currently scheduled.
    #[must_use]
    pub fn send_in_progress(self) -> bool {
        self.get(Self::SEND_IN_PROGRESS)
    }
    /// Update the send-in-progress flag.
    pub fn set_send_in_progress(&mut self, value: bool) {
        self.set_bit(Self::SEND_IN_PROGRESS, value);
    }
}

/// Internal connection state for simulation
#[derive(Debug, Clone)]
pub struct ConnectionState {
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

    /// Next available time for sending messages from this connection.
    pub next_send_time: Duration,

    /// Boolean flags (closure, chaos, send-in-progress) packed into a bitfield.
    /// Use the named accessor methods on [`ConnectionFlags`] to inspect or
    /// update individual bits.
    pub flags: ConnectionFlags,

    /// When the cut expires and the connection is restored (in simulation time).
    /// Only meaningful when `flags.is_cut()` is true.
    pub cut_expiry: Option<Duration>,

    /// Reason for connection closure - distinguishes FIN vs RST semantics.
    /// When `flags.is_closed()` is true, this indicates whether it was graceful or aborted.
    pub close_reason: CloseReason,

    /// Send buffer capacity in bytes.
    /// When the send buffer reaches this limit, `poll_write` returns Pending.
    /// Calculated from BDP (Bandwidth-Delay Product): latency × bandwidth.
    pub send_buffer_capacity: usize,

    /// Per-connection send delay override (asymmetric latency).
    /// If set, this delay is applied when sending data from this connection.
    /// If None, the global `write_latency` from `NetworkConfiguration` is used.
    pub send_delay: Option<Duration>,

    /// Per-connection receive delay override (asymmetric latency).
    /// If set, this delay is applied when receiving data on this connection.
    /// If None, the global `read_latency` from `NetworkConfiguration` is used.
    pub recv_delay: Option<Duration>,

    /// When a half-open connection starts returning errors (in simulation time).
    /// Before this time: writes succeed (data dropped), reads block.
    /// After this time: both read and write return ECONNRESET.
    pub half_open_error_at: Option<Duration>,

    /// Time of the most recently scheduled `DataDelivery` event from this connection.
    /// Used to ensure `FinDelivery` is scheduled after the last data arrives at the peer.
    pub last_data_delivery_scheduled_at: Option<Duration>,
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
    pub connections: BTreeMap<ConnectionId, ConnectionState>,
    /// Active listener IDs.
    pub listeners: BTreeSet<ListenerId>,
    /// Connections pending acceptance, indexed by address.
    pub pending_connections: BTreeMap<String, ConnectionId>,

    /// Write clog state (temporary write blocking).
    pub connection_clogs: BTreeMap<ConnectionId, ClogState>,

    /// Read clog state (temporary read blocking, symmetric with write clogging).
    pub read_clogs: BTreeMap<ConnectionId, ClogState>,

    /// Partitions between specific IP pairs (from, to) -> partition state
    pub ip_partitions: BTreeMap<(IpAddr, IpAddr), PartitionState>,
    /// Send partitions - IP cannot send to anyone
    pub send_partitions: BTreeMap<IpAddr, Duration>,
    /// Receive partitions - IP cannot receive from anyone
    pub recv_partitions: BTreeMap<IpAddr, Duration>,

    /// Last time a random close was triggered (global cooldown tracking)
    /// FDB: g_simulator->lastConnectionFailure - see sim2.actor.cpp:583
    pub last_random_close_time: Duration,

    /// Per-IP-pair base latencies for consistent connection behavior.
    /// Once set on first connection, all subsequent connections between the same
    /// IP pair will use this base latency (with optional jitter on top).
    pub pair_latencies: BTreeMap<(IpAddr, IpAddr), Duration>,
}

impl NetworkState {
    /// Create a new network state with the given configuration.
    #[must_use]
    pub fn new(config: NetworkConfiguration) -> Self {
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

    /// Extract IP address from a network address string.
    /// Supports formats like "127.0.0.1:8080", "\[`::1`\]:8080", etc.
    #[must_use]
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
    #[must_use]
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
    #[must_use]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    pub pending_ops: BTreeMap<u64, PendingStorageOp>,
    /// Next sequence number for operations on this file
    pub next_op_seq: u64,
    /// IP address of the process that owns this file.
    pub owner_ip: IpAddr,
}

impl StorageFileState {
    /// Create a new storage file state.
    #[must_use]
    pub fn new(
        id: FileId,
        path: String,
        options: OpenOptions,
        storage: InMemoryStorage,
        owner_ip: IpAddr,
    ) -> Self {
        Self {
            id,
            path,
            position: 0,
            options,
            storage,
            is_closed: false,
            pending_ops: BTreeMap::new(),
            next_op_seq: 0,
            owner_ip,
        }
    }
}

/// Storage-related state management for the simulation.
#[derive(Debug)]
pub struct StorageState {
    /// Counter for generating unique file IDs
    pub next_file_id: u64,
    /// Default storage configuration (used when no per-process config is set)
    pub config: StorageConfiguration,
    /// Per-process storage configurations, keyed by IP address.
    ///
    /// When a file's owner IP has an entry here, that config is used for
    /// fault injection and latency calculations instead of the global default.
    pub per_process_configs: HashMap<IpAddr, StorageConfiguration>,
    /// Active files indexed by their ID
    pub files: BTreeMap<FileId, StorageFileState>,
    /// Mapping from path to file ID for quick lookup
    pub path_to_file: BTreeMap<String, FileId>,
    /// Set of paths that have been deleted (for `create_new` semantics)
    pub deleted_paths: HashSet<String>,
    /// Set of (`file_id`, `op_seq`) pairs for sync operations that failed
    pub sync_failures: HashSet<(FileId, u64)>,
}

impl StorageState {
    /// Create a new storage state with the given configuration.
    #[must_use]
    pub fn new(config: StorageConfiguration) -> Self {
        Self {
            next_file_id: 0,
            config,
            per_process_configs: HashMap::new(),
            files: BTreeMap::new(),
            path_to_file: BTreeMap::new(),
            deleted_paths: HashSet::new(),
            sync_failures: HashSet::new(),
        }
    }

    /// Resolve storage configuration for a given IP address.
    ///
    /// Returns the per-process config if one is set, otherwise the global default.
    #[must_use]
    pub fn config_for(&self, ip: IpAddr) -> &StorageConfiguration {
        self.per_process_configs.get(&ip).unwrap_or(&self.config)
    }
}

impl Default for StorageState {
    fn default() -> Self {
        Self::new(StorageConfiguration::default())
    }
}
