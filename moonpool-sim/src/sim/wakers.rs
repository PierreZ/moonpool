//! Waker management for async coordination.
//!
//! This module provides the WakerRegistry for managing task wakers
//! in the simulation environment.

use std::collections::BTreeMap;
use std::task::Waker;

use crate::network::sim::{ConnectionId, ListenerId};
use crate::sim::state::FileId;

/// Waker management for async coordination.
#[derive(Debug, Default)]
pub struct WakerRegistry {
    #[allow(dead_code)] // Will be used for connection coordination in future phases
    pub(crate) connection_wakers: BTreeMap<ConnectionId, Waker>,
    pub(crate) listener_wakers: BTreeMap<ListenerId, Waker>,
    pub(crate) read_wakers: BTreeMap<ConnectionId, Waker>,
    pub(crate) task_wakers: BTreeMap<u64, Waker>,
    /// Wakers waiting for write clog to clear
    pub(crate) clog_wakers: BTreeMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for read clog to clear
    pub(crate) read_clog_wakers: BTreeMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for cut connections to be restored
    pub(crate) cut_wakers: BTreeMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for send buffer space to become available
    pub(crate) send_buffer_wakers: BTreeMap<ConnectionId, Vec<Waker>>,
    /// Wakers waiting for storage operations to complete.
    /// Keyed by (FileId, operation_sequence_number).
    pub(crate) storage_wakers: BTreeMap<(FileId, u64), Waker>,
}
