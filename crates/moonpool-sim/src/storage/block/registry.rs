//! Per-process block-store registry owned by `SimWorld`.
//!
//! Each process IP gets its own [`SimBlockStore`], created lazily on first
//! access. A store holds no randomness of its own: every fault it injects is a
//! draw on the simulation's one stream at the moment the operation runs, so
//! creating a store consumes nothing and the per-process split is purely about
//! which devices belong to which process.

use std::collections::BTreeMap;
use std::net::IpAddr;

use super::{BlockFaultConfig, SimBlockStore};

/// Lazily created per-process block stores plus the fault configuration new
/// stores are born with.
#[derive(Debug, Default)]
pub(crate) struct BlockDeviceRegistry {
    config: BlockFaultConfig,
    stores: BTreeMap<IpAddr, SimBlockStore>,
}

impl BlockDeviceRegistry {
    /// Replace the fault configuration used for stores created *after* this
    /// call. The builder sets it right after constructing the world, before
    /// any process runs.
    pub(crate) fn set_config(&mut self, config: BlockFaultConfig) {
        self.config = config;
    }

    /// Stop injecting new faults on every block store: the configuration
    /// future stores are born with, and every store already created.
    ///
    /// Damage already written to a device stays (see
    /// [`BlockFaultConfig::disable_fault_injection`]).
    pub(crate) fn disable_fault_injection(&mut self) {
        self.config.disable_fault_injection();
        for store in self.stores.values() {
            store.disable_fault_injection();
        }
    }

    /// The store for `ip`, created on first access.
    pub(crate) fn store_for(&mut self, ip: IpAddr) -> SimBlockStore {
        self.stores
            .entry(ip)
            .or_insert_with(|| SimBlockStore::new(self.config.clone()))
            .clone()
    }

    /// The store for `ip` if one was ever created (crash/wipe must not
    /// instantiate stores for processes that never used block devices).
    pub(crate) fn existing_store(&self, ip: IpAddr) -> Option<SimBlockStore> {
        self.stores.get(&ip).cloned()
    }
}
