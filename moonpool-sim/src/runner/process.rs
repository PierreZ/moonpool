//! Process trait and reboot types for simulation testing.
//!
//! Processes represent the **system under test** — server nodes that can be
//! killed and restarted (rebooted). Each process gets fresh in-memory state
//! on every boot; persistence is only through storage.
//!
//! This is separate from [`Workload`](super::workload::Workload), which
//! represents the **test driver** that survives server reboots.
//!
//! # Usage
//!
//! ```ignore
//! use moonpool_sim::{Process, SimContext, SimulationResult};
//!
//! struct PaxosNode;
//!
//! #[async_trait]
//! impl Process for PaxosNode {
//!     fn name(&self) -> &str { "paxos" }
//!     async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
//!         let role = ctx.topology().my_tags().get("role")
//!             .ok_or_else(|| moonpool_sim::SimulationError::InvalidState("missing role tag".into()))?;
//!         // Run based on assigned role from tags...
//!         Ok(())
//!     }
//! }
//! ```

use std::ops::Range;

use async_trait::async_trait;

use crate::SimulationResult;

use super::context::SimContext;

/// A process that participates in simulation as part of the system under test.
///
/// Processes are the primary unit of server behavior. A fresh instance is created
/// from the factory on every boot (first boot and every reboot). State only
/// persists through storage, not in-memory fields.
///
/// The process reads its tags and index from [`SimContext`] to determine its role.
#[async_trait]
pub trait Process: Send + Sync + 'static {
    /// Name of this process type for reporting.
    fn name(&self) -> &str;

    /// Run the process. Called on each boot (first boot and every reboot).
    ///
    /// The [`SimContext`] has fresh providers each boot. The process should
    /// bind listeners, establish connections, and run its main loop.
    ///
    /// Returns when the process exits voluntarily, or gets cancelled on reboot.
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()>;
}

/// The type of reboot to perform on a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebootKind {
    /// Signal shutdown token, wait grace period, drain send buffers, then restart.
    ///
    /// The process's `ctx.shutdown()` token fires. The process has a grace period
    /// to finish up. If it doesn't exit in time, the task is force-cancelled.
    /// Send buffers drain during the grace period (FIN delivery).
    Graceful,

    /// Instant kill: task cancelled, all connections abort immediately.
    ///
    /// No buffer drain. Peers see connection reset errors. Unsynced storage
    /// data may be lost (when per-IP storage scoping is implemented).
    Crash,

    /// Instant kill + wipe all storage for this process.
    ///
    /// Same as [`Crash`](RebootKind::Crash) but also deletes all persistent
    /// storage owned by this process's IP. Simulates total data loss or a
    /// new node joining the cluster.
    CrashAndWipe,
}

/// Built-in attrition configuration for automatic process reboots.
///
/// Provides a default chaos mechanism that randomly kills and restarts server
/// processes during the chaos phase. For custom fault injection strategies,
/// implement [`FaultInjector`](super::fault_injector::FaultInjector) instead.
///
/// # Probabilities
///
/// The `prob_*` fields are weights that get normalized internally. They don't
/// need to sum to 1.0, but all must be non-negative.
///
/// # Example
///
/// ```ignore
/// Attrition {
///     max_dead: 1,
///     prob_graceful: 0.3,
///     prob_crash: 0.5,
///     prob_wipe: 0.2,
///     recovery_delay_ms: None,
///     grace_period_ms: None,
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Attrition {
    /// Maximum number of simultaneously dead processes.
    ///
    /// The attrition injector will not kill a process if the number of currently
    /// dead (not yet restarted) processes is already at this limit.
    pub max_dead: usize,

    /// Weight for [`RebootKind::Graceful`] reboots.
    pub prob_graceful: f64,

    /// Weight for [`RebootKind::Crash`] reboots.
    pub prob_crash: f64,

    /// Weight for [`RebootKind::CrashAndWipe`] reboots.
    pub prob_wipe: f64,

    /// Recovery delay range in milliseconds.
    ///
    /// After a process is killed (crash or force-kill after grace), it restarts
    /// after a seeded random delay drawn from this range.
    ///
    /// Defaults to `1000..10000` (1-10 seconds) if not set.
    pub recovery_delay_ms: Option<Range<usize>>,

    /// Grace period range in milliseconds (for graceful reboots).
    ///
    /// After the per-process shutdown token is cancelled, the process has this
    /// long to clean up before being force-killed. The actual duration is a
    /// seeded random value from this range.
    ///
    /// Defaults to `2000..5000` (2-5 seconds) if not set.
    pub grace_period_ms: Option<Range<usize>>,
}

impl Attrition {
    /// Choose a [`RebootKind`] based on the configured probabilities using the
    /// given random value in `[0.0, 1.0)`.
    pub(crate) fn choose_kind(&self, rand_val: f64) -> RebootKind {
        let total = self.prob_graceful + self.prob_crash + self.prob_wipe;
        if total <= 0.0 {
            return RebootKind::Crash;
        }

        let normalized = rand_val * total;
        if normalized < self.prob_graceful {
            RebootKind::Graceful
        } else if normalized < self.prob_graceful + self.prob_crash {
            RebootKind::Crash
        } else {
            RebootKind::CrashAndWipe
        }
    }

    /// Derive a per-seed swarm reboot regime from this base configuration.
    ///
    /// Implements *swarm testing* (Groce et al., ISSTA 2012) for attrition: each
    /// seed exercises a random reboot *regime* rather than the fixed configured
    /// one. This surfaces bug classes that a single fixed regime hides — most
    /// importantly the **never-reboot** case (needed to find slow leaks / timer
    /// overflows) and single-mode cases ("always crash", "graceful-only").
    ///
    /// Draws exactly four `config_random_bool` values from the independent
    /// `CONFIG_RNG` stream (fixed sequence ⇒ reproducible per seed, never
    /// perturbs in-run randomness). The first draw, with ~50% probability, sets
    /// `max_dead = 0` — the never-reboot regime, where the injector's
    /// `dead_count() >= max_dead` gate is always true so it never reboots. The
    /// remaining three each mask one reboot-kind weight to `0.0` with ~50%
    /// probability.
    ///
    /// When all three kind weights are masked off, [`choose_kind`](Self::choose_kind)
    /// falls back to [`RebootKind::Crash`] — the "always crash" single-mode regime.
    #[must_use]
    pub fn swarm_for_seed(&self) -> Attrition {
        let mut regime = self.clone();
        if !crate::sim::config_random_bool(0.5) {
            regime.max_dead = 0;
        }
        if !crate::sim::config_random_bool(0.5) {
            regime.prob_graceful = 0.0;
        }
        if !crate::sim::config_random_bool(0.5) {
            regime.prob_crash = 0.0;
        }
        if !crate::sim::config_random_bool(0.5) {
            regime.prob_wipe = 0.0;
        }
        regime
    }
}

#[cfg(test)]
mod swarm_tests {
    use super::Attrition;
    use crate::sim::rng::set_config_seed;

    /// A representative base regime: all three reboot kinds enabled.
    fn base() -> Attrition {
        Attrition {
            max_dead: 2,
            prob_graceful: 0.3,
            prob_crash: 0.5,
            prob_wipe: 0.2,
            recovery_delay_ms: None,
            grace_period_ms: None,
        }
    }

    /// Build a swarm regime the way the runner does: config stream seeded per iteration.
    fn swarm_for(seed: u64) -> Attrition {
        set_config_seed(seed);
        base().swarm_for_seed()
    }

    #[test]
    fn swarm_regime_is_deterministic_per_seed() {
        for seed in [0_u64, 1, 42, 12_345] {
            assert_eq!(
                swarm_for(seed),
                swarm_for(seed),
                "swarm regime must be reproducible for seed {seed}"
            );
        }
    }

    #[test]
    fn swarm_reaches_never_reboot() {
        let saw_never_reboot = (0..1000_u64).any(|s| swarm_for(s).max_dead == 0);
        assert!(
            saw_never_reboot,
            "no seed in 0..1000 produced the never-reboot regime (max_dead == 0)"
        );
    }

    #[test]
    fn swarm_reaches_single_mode() {
        let saw_single_mode = (0..1000_u64).any(|s| {
            let r = swarm_for(s);
            let on = [r.prob_graceful, r.prob_crash, r.prob_wipe]
                .iter()
                .filter(|&&w| w > 0.0)
                .count();
            on == 1
        });
        assert!(
            saw_single_mode,
            "no seed in 0..1000 produced a single-mode regime (one kind enabled)"
        );
    }
}
