//! Simulation context provided to workloads.
//!
//! `SimContext` wraps providers, topology, shared state, and shutdown signaling
//! into a single ergonomic handle that workloads receive.

use std::cell::Cell;
use std::rc::Rc;

use crate::chaos::state_handle::StateHandle;
use crate::network::SimNetworkProvider;
use crate::providers::{SimRandomProvider, SimTimeProvider};
use crate::storage::SimStorageProvider;

/// Simple cancellation token using `Rc<Cell<bool>>`.
///
/// Used to signal shutdown to all workloads. Cheaper than
/// `tokio_util::sync::CancellationToken` and `!Send`-compatible.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Rc<Cell<bool>>,
}

impl CancellationToken {
    /// Create a new cancellation token (not cancelled).
    pub fn new() -> Self {
        Self {
            cancelled: Rc::new(Cell::new(false)),
        }
    }

    /// Cancel this token. All clones will observe the cancellation.
    pub fn cancel(&self) {
        self.cancelled.set(true);
    }

    /// Check if this token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.get()
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Context provided to each workload during simulation.
///
/// Bundles all providers, topology information, shared state, and shutdown
/// signaling into a single handle.
pub struct SimContext {
    network: SimNetworkProvider,
    time: SimTimeProvider,
    random: SimRandomProvider,
    storage: SimStorageProvider,
    my_ip: String,
    peers: Vec<String>,
    shutdown: CancellationToken,
    state: StateHandle,
}

impl SimContext {
    /// Create a new simulation context.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        network: SimNetworkProvider,
        time: SimTimeProvider,
        random: SimRandomProvider,
        storage: SimStorageProvider,
        my_ip: String,
        peers: Vec<String>,
        shutdown: CancellationToken,
        state: StateHandle,
    ) -> Self {
        Self {
            network,
            time,
            random,
            storage,
            my_ip,
            peers,
            shutdown,
            state,
        }
    }

    /// Get the network provider.
    pub fn network(&self) -> &SimNetworkProvider {
        &self.network
    }

    /// Get the time provider.
    pub fn time(&self) -> &SimTimeProvider {
        &self.time
    }

    /// Get the random provider.
    pub fn random(&self) -> &SimRandomProvider {
        &self.random
    }

    /// Get the storage provider.
    pub fn storage(&self) -> &SimStorageProvider {
        &self.storage
    }

    /// Get this workload's address string (used for bind/connect).
    pub fn my_ip(&self) -> &str {
        &self.my_ip
    }

    /// Get the first peer's address (convenience for two-node topologies).
    ///
    /// # Panics
    ///
    /// Panics if there are no peers.
    pub fn peer(&self) -> &str {
        &self.peers[0]
    }

    /// Get all peer addresses.
    pub fn peers(&self) -> &[String] {
        &self.peers
    }

    /// Get the shutdown cancellation token.
    pub fn shutdown(&self) -> &CancellationToken {
        &self.shutdown
    }

    /// Get the shared state handle.
    pub fn state(&self) -> &StateHandle {
        &self.state
    }
}
