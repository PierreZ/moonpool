//! Per-process block-store registry owned by `SimWorld`.
//!
//! Each process IP gets its own [`SimBlockStore`], created lazily on first
//! access with a seed derived as a **pure function** of the iteration seed and
//! the IP (via `splitmix64`) — no RNG stream is consumed, so whether or when a
//! process first touches a block device can never shift the counted sim RNG
//! stream or fork-explorer replay.

use std::collections::BTreeMap;
use std::net::IpAddr;

use super::{BlockFaultConfig, SimBlockStore};
use crate::sim::rng::splitmix64;

/// Salt mixed into the iteration seed when deriving per-process block store
/// seeds, decorrelating them from the other salted streams.
const BLOCK_STORE_SALT: u64 = 0x626C_6F63_6B64_6576; // "blockdev"

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
            .or_insert_with(|| {
                let seed = splitmix64(
                    crate::sim::rng::current_sim_seed() ^ BLOCK_STORE_SALT ^ ip_seed_component(ip),
                );
                SimBlockStore::new(seed, self.config.clone())
            })
            .clone()
    }

    /// The store for `ip` if one was ever created (crash/wipe must not
    /// instantiate stores for processes that never used block devices).
    pub(crate) fn existing_store(&self, ip: IpAddr) -> Option<SimBlockStore> {
        self.stores.get(&ip).cloned()
    }
}

/// Fold an IP address into a stable 64-bit seed component.
fn ip_seed_component(ip: IpAddr) -> u64 {
    match ip {
        IpAddr::V4(v4) => u64::from(u32::from(v4)),
        IpAddr::V6(v6) => {
            let octets = v6.octets();
            let low = u64::from_le_bytes(octets[0..8].try_into().expect("8 bytes"));
            let high = u64::from_le_bytes(octets[8..16].try_into().expect("8 bytes"));
            splitmix64(low) ^ high
        }
    }
}
