//! Waker management for async coordination.
//!
//! This module provides the WakerRegistry for managing task wakers
//! in the simulation environment.

use std::collections::HashMap;
use std::task::Waker;

use crate::network::sim::{ConnectionId, ListenerId};
use crate::sim::state::FileId;

/// Waker management for async coordination.
#[derive(Debug, Default)]
pub struct WakerRegistry {
    #[allow(dead_code)] // Will be used for connection coordination in future phases
    pub(crate) connection_wakers: HashMap<ConnectionId, Waker>,
    pub(crate) listener_wakers: HashMap<ListenerId, Waker>,
    pub(crate) read_wakers: HashMap<ConnectionId, Waker>,
    pub(crate) task_wakers: HashMap<u64, Waker>,
    /// Wakers waiting for write clog to clear
    pub(crate) clog_wakers: HashMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for read clog to clear
    pub(crate) read_clog_wakers: HashMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for cut connections to be restored
    pub(crate) cut_wakers: HashMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for send buffer space to become available
    pub(crate) send_buffer_wakers: HashMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for storage operations to complete.
    /// Keyed by (FileId, operation_sequence_number).
    pub(crate) storage_wakers: HashMap<(FileId, u64), Waker>,
}
