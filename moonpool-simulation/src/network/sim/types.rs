use std::collections::VecDeque;

/// Unique identifier for network connections
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(pub(crate) u64);

/// Unique identifier for listeners
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListenerId(pub(crate) u64);

/// Internal connection state
#[allow(dead_code)] // Infrastructure for future phases
#[derive(Debug)]
pub(crate) struct ConnectionState {
    /// Unique identifier for this connection
    pub id: ConnectionId,
    /// Address this connection is associated with
    pub addr: String,
    /// Buffer of received data waiting to be read
    pub receive_buffer: VecDeque<u8>,
}

/// Internal listener state
#[allow(dead_code)] // Infrastructure for future phases
#[derive(Debug)]
pub(crate) struct ListenerState {
    /// Unique identifier for this listener
    pub id: ListenerId,
    /// Address this listener is bound to
    pub addr: String,
    /// Queue of connections waiting to be accepted
    pub pending_connections: VecDeque<ConnectionId>,
}
