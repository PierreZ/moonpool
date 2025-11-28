//! Waker management for async coordination.
//!
//! This module provides the WakerRegistry for managing task wakers
//! in the simulation environment.

use std::collections::HashMap;
use std::task::Waker;

use crate::network::sim::{ConnectionId, ListenerId};

/// Waker management for async coordination.
#[derive(Debug, Default)]
pub struct WakerRegistry {
    #[allow(dead_code)] // Will be used for connection coordination in future phases
    pub(crate) connection_wakers: HashMap<ConnectionId, Waker>,
    pub(crate) listener_wakers: HashMap<ListenerId, Waker>,
    pub(crate) read_wakers: HashMap<ConnectionId, Waker>,
    pub(crate) task_wakers: HashMap<u64, Waker>,
    pub(crate) clog_wakers: HashMap<ConnectionId, Vec<Waker>>,
}
