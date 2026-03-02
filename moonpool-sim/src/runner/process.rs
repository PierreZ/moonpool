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
//! #[async_trait(?Send)]
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
#[async_trait(?Send)]
pub trait Process: 'static {
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
    /// storage. Simulates total data loss or a new node joining the cluster.
    ///
    /// **Note**: Storage wipe is deferred to future work (storage not yet scoped
    /// per IP). Currently behaves the same as `Crash`.
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
}
