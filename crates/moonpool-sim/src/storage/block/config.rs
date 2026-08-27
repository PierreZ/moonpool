//! Fault and crash-model configuration for the simulated block device.

use crate::sim::rng::config_random_bool;

/// Configuration of the simulated block device's fault families and
/// barrier-bounded crash model.
///
/// All fault families are **off by default** (probability `0.0`); crash-shape
/// parameters default to the reference values from the crash-model precedents
/// (FDB `AsyncFileNonDurable` uses a 10% fully-clean crash).
///
/// Fault families are gated by BOTH this configuration (typically driven by a
/// per-seed swarm subset, see [`BlockFaultConfig::swarm`]) AND the
/// caller-provided eligibility mask installed with
/// [`SimBlockStore::set_eligibility_mask`](super::SimBlockStore::set_eligibility_mask).
#[derive(Debug, Clone)]
pub struct BlockFaultConfig {
    /// Probability that a read returns [`BlockError::Io`](moonpool_core::BlockError::Io)
    /// (EIO as an *operating condition*, not corrupt bytes).
    pub eio_read_probability: f64,
    /// Probability that a write returns [`BlockError::Io`](moonpool_core::BlockError::Io).
    pub eio_write_probability: f64,
    /// Per-sector probability that a read plants a latent fault: the sector's
    /// content is deterministically corrupted at read time, identically on
    /// every retry.
    pub read_corruption_probability: f64,
    /// Probability that a write is misdirected to a different sector-aligned
    /// offset within the same region.
    pub misdirected_write_probability: f64,
    /// Probability that a write is a phantom: it completes successfully but
    /// its bytes never reach the device (reads keep seeing the old contents).
    pub phantom_write_probability: f64,
    /// Probability that a `persist()` call fails with an I/O error. Buffered
    /// writes stay volatile; the caller decides whether to retry or fail-stop.
    pub persist_failure_probability: f64,

    /// Probability that a crash is fully clean: every buffered write survives
    /// intact. FDB uses `p = 0.1`.
    pub clean_crash_probability: f64,
    /// Probability that a (non-clean) crash rolls back a contiguous run of
    /// sectors together — correlated erase-block damage (Zheng, FAST'13).
    pub correlated_rollback_probability: f64,
    /// Maximum sector-run length for a correlated rollback.
    pub correlated_rollback_max_run: u64,
    /// Per-sector probability that a buffered sector resolves to *lost* on
    /// crash: it reverts to never-written and reads the device's fill pattern
    /// (zeros or garbage, chosen per seed).
    pub crash_lost_probability: f64,
    /// Per-sector probability that a buffered sector resolves with a *latent
    /// read fault* on crash: the new contents land, but reads return
    /// deterministically corrupted bytes.
    pub crash_latent_fault_probability: f64,
    /// Per-sector probability that a buffered sector is *shorn* on crash: a
    /// sub-sector prefix/suffix mix of old and new bytes.
    ///
    /// **Off by default**: enabling it deliberately weakens the "atomicity
    /// unit is one sector" contract clause to model pre-AWUPF drives and
    /// RAID-split shorn writes (Zheng, FAST'13).
    pub shorn_write_probability: f64,
    /// Probability that a region size grown since the last `persist()`
    /// survives a crash (the grow itself, not its contents).
    pub grow_survives_crash_probability: f64,

    /// Opt-in **barrier violation** family (off by default; give it its own
    /// swarm slot): per-sector probability that `persist()` *lies* — it
    /// reports a sector durable while leaving it volatile, so a later crash
    /// loses or reorders a synced write (fsyncgate class; Zheng's
    /// unserializable writes). With this family armed, the lost-synced-write
    /// oracle flips from hard-failure to must-detect mode.
    pub barrier_violation_probability: f64,

    /// Probability that a device fills never-written and lost sectors with
    /// deterministic garbage instead of zeros. Decided once per device from
    /// the store's seeded RNG; zeros are the dangerous real-world case (SATA
    /// `RZAT` / `NVMe` `DLFEAT` / unwritten extents).
    pub garbage_fill_probability: f64,
}

impl Default for BlockFaultConfig {
    fn default() -> Self {
        Self {
            eio_read_probability: 0.0,
            eio_write_probability: 0.0,
            read_corruption_probability: 0.0,
            misdirected_write_probability: 0.0,
            phantom_write_probability: 0.0,
            persist_failure_probability: 0.0,
            clean_crash_probability: 0.1,
            correlated_rollback_probability: 0.25,
            correlated_rollback_max_run: 8,
            crash_lost_probability: 0.1,
            crash_latent_fault_probability: 0.05,
            shorn_write_probability: 0.0,
            grow_survives_crash_probability: 0.5,
            barrier_violation_probability: 0.0,
            garbage_fill_probability: 0.5,
        }
    }
}

impl BlockFaultConfig {
    /// A profile with every default-on fault family enabled at moderate
    /// probability, for chaos sweeps. Barrier violation and shorn writes stay
    /// off — arm them explicitly.
    #[must_use]
    pub fn chaos() -> Self {
        Self {
            eio_read_probability: 0.01,
            eio_write_probability: 0.01,
            read_corruption_probability: 0.005,
            misdirected_write_probability: 0.005,
            phantom_write_probability: 0.005,
            persist_failure_probability: 0.01,
            ..Self::default()
        }
    }

    /// A crash profile for an AWUPF-compliant atomic-sector disk: buffered
    /// sectors resolve strictly to old or new contents (no loss, no latent
    /// faults, no shorn writes). Reordering across the barrier window and
    /// torn multi-sector writes remain fully in play.
    #[must_use]
    pub fn atomic_sectors(mut self) -> Self {
        self.crash_lost_probability = 0.0;
        self.crash_latent_fault_probability = 0.0;
        self.shorn_write_probability = 0.0;
        self
    }

    /// Apply a per-seed swarm subset: each enabled fault family is
    /// independently kept or zeroed with probability 0.5, drawn from the
    /// configuration RNG stream (never the counted sim RNG). The barrier
    /// violation family occupies its own swarm slot.
    #[must_use]
    pub fn swarm(mut self) -> Self {
        for probability in [
            &mut self.eio_read_probability,
            &mut self.eio_write_probability,
            &mut self.read_corruption_probability,
            &mut self.misdirected_write_probability,
            &mut self.phantom_write_probability,
            &mut self.persist_failure_probability,
            &mut self.barrier_violation_probability,
        ] {
            // Always consume exactly one draw per family so the config stream
            // stays aligned regardless of which families start enabled.
            let keep = config_random_bool(0.5);
            if !keep {
                *probability = 0.0;
            }
        }
        self
    }
}
