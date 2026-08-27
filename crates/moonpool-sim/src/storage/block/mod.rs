//! Simulated region-based block device with a barrier-bounded crash model.
//!
//! Implements the [`moonpool_core::BlockDevice`] contract (issue #184): a
//! sector-addressed, region-based device whose simulated crashes can produce
//! every state the contract permits — and, opt-in, the states a *lying* disk
//! produces:
//!
//! - **Torn multi-sector writes**: each sector of a write buffered since the
//!   last `persist()` independently resolves to old, new, lost, or damaged.
//! - **Reordering across the barrier window**: a later write can survive a
//!   crash intact while an earlier (unpersisted) one is torn — the exact
//!   shape CTRL-style journal recovery exists to survive.
//! - **Zero-fill AND garbage-fill** for never-written and lost sectors,
//!   chosen per seed, so content-sniffing recovery bugs cannot hide.
//! - **Fault families**: EIO on read/write (errors, not corrupt bytes),
//!   read-time latent corruption (deterministic on retry), misdirected
//!   writes contained within a region, phantom writes, persist failures.
//! - **Barrier violations** (off by default): `persist()` occasionally lies
//!   about a sector, and the lost-synced-write oracle flips from
//!   hard-failure to must-detect mode.
//!
//! Entry points: [`SimBlockStore`] owns state, faults, and crashes;
//! [`SimBlockDeviceProvider`] / [`SimBlockDevice`] implement the provider
//! traits on top of it.

mod config;
mod device;
mod store;

pub use config::BlockFaultConfig;
pub use device::{SimBlockDevice, SimBlockDeviceProvider};
pub use store::{
    BlockCrashOutcome, BlockCrashReport, BlockEligibilityMask, BlockFaultKind, BlockFaultRecord,
    BlockSectorResolution, EioTarget, SimBlockStore,
};
