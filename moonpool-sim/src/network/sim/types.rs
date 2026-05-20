/// Unique identifier for network connections
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(pub(crate) u64);

/// Unique identifier for listeners
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListenerId(pub(crate) u64);
