//! Waker management for async coordination.
//!
//! This module provides the `WakerRegistry` for managing task wakers
//! in the simulation environment.

use std::collections::BTreeMap;
use std::task::Waker;

use crate::network::sim::{ConnectionId, ListenerId};
use crate::sim::state::FileId;

/// Waker management for async coordination.
#[derive(Debug, Default)]
pub struct WakerRegistry {
    /// Wakers waiting on `accept()` per listener.
    pub(crate) listeners: BTreeMap<ListenerId, Waker>,
    /// Wakers waiting on `read` per connection.
    pub(crate) reads: BTreeMap<ConnectionId, Waker>,
    /// Wakers waiting on time-based events per task id.
    pub(crate) tasks: BTreeMap<u64, Waker>,
    /// Wakers waiting for write clog to clear.
    pub(crate) write_clogs: BTreeMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for read clog to clear.
    pub(crate) read_clogs: BTreeMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for cut connections to be restored.
    pub(crate) cuts: BTreeMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for send buffer space to become available.
    pub(crate) send_buffers: BTreeMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for storage operations to complete.
    /// Keyed by (`FileId`, `operation_sequence_number`).
    pub(crate) storage_ops: BTreeMap<(FileId, u64), Waker>,
}
