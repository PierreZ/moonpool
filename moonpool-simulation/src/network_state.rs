use std::{
    collections::{HashMap, VecDeque},
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

/// Connection cutting state - tracks cut connections for restoration
#[derive(Debug, Clone)]
pub struct CutState {
    /// Original connection data (for restoration)
    pub connection_data: ConnectionState,
    /// When connection can be restored (in simulation time)
    pub reconnect_at: Duration,
    /// Cut count for this connection
    #[allow(dead_code)] // Will be used for cut statistics and limiting reconnection attempts
    pub cut_count: u32,
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
    pub cut_connections: HashMap<ConnectionId, CutState>,
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
            cut_connections: HashMap::new(),
        }
    }
}
