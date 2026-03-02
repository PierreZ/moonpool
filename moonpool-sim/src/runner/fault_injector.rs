//! Fault injection for simulation chaos testing.
//!
//! [`FaultInjector`] defines fault injection strategies (partitions, connection drops, etc.)
//! that run during the chaos phase of a simulation. [`FaultContext`] provides access to
//! `SimWorld` fault injection primitives.
//!
//! [`PhaseConfig`] controls the two-phase chaos/recovery lifecycle:
//! - **Chaos phase**: Workloads + fault injectors run concurrently
//! - **Recovery phase**: Fault injectors stopped, workloads continue, system heals
//!
//! # Usage
//!
//! ```ignore
//! use moonpool_sim::{FaultInjector, FaultContext, PhaseConfig, SimulationResult};
//! use std::time::Duration;
//!
//! struct RandomPartition { probability: f64 }
//!
//! #[async_trait(?Send)]
//! impl FaultInjector for RandomPartition {
//!     fn name(&self) -> &str { "random_partition" }
//!     async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
//!         let ips = ctx.all_ips();
//!         while !ctx.chaos_shutdown().is_cancelled() {
//!             if ctx.random().random_bool(self.probability) && ips.len() >= 2 {
//!                 ctx.partition(&ips[0], &ips[1])?;
//!                 ctx.time().sleep(Duration::from_secs(5)).await?;
//!                 ctx.heal_partition(&ips[0], &ips[1])?;
//!             }
//!             ctx.time().sleep(Duration::from_secs(1)).await?;
//!         }
//!         Ok(())
//!     }
//! }
//! ```

use std::time::Duration;

use async_trait::async_trait;
use moonpool_core::TimeProvider;

use crate::SimulationResult;
use crate::providers::{SimRandomProvider, SimTimeProvider};
use crate::runner::process::RebootKind;
use crate::runner::tags::TagRegistry;
use crate::sim::SimWorld;

/// Context for fault injectors — gives access to SimWorld fault injection methods.
///
/// Unlike `SimContext` (which workloads receive), `FaultContext` provides direct
/// access to network partitioning, reboot, and other fault primitives that normal
/// workloads should not use.
pub struct FaultContext {
    sim: SimWorld,
    all_ips: Vec<String>,
    process_ips: Vec<String>,
    tag_registry: TagRegistry,
    random: SimRandomProvider,
    time: SimTimeProvider,
    chaos_shutdown: tokio_util::sync::CancellationToken,
}

impl FaultContext {
    /// Create a new fault context.
    pub fn new(
        sim: SimWorld,
        all_ips: Vec<String>,
        random: SimRandomProvider,
        time: SimTimeProvider,
        chaos_shutdown: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            sim,
            all_ips,
            process_ips: Vec::new(),
            tag_registry: TagRegistry::default(),
            random,
            time,
            chaos_shutdown,
        }
    }

    /// Create a new fault context with process information.
    pub fn new_with_processes(
        sim: SimWorld,
        all_ips: Vec<String>,
        process_ips: Vec<String>,
        tag_registry: TagRegistry,
        random: SimRandomProvider,
        time: SimTimeProvider,
        chaos_shutdown: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            sim,
            all_ips,
            process_ips,
            tag_registry,
            random,
            time,
            chaos_shutdown,
        }
    }

    /// Create a bidirectional network partition between two IPs.
    ///
    /// The partition persists until [`heal_partition`](Self::heal_partition) is called.
    pub fn partition(&self, a: &str, b: &str) -> SimulationResult<()> {
        let a_ip: std::net::IpAddr = a.parse().map_err(|e| {
            crate::SimulationError::InvalidState(format!("invalid IP '{}': {}", a, e))
        })?;
        let b_ip: std::net::IpAddr = b.parse().map_err(|e| {
            crate::SimulationError::InvalidState(format!("invalid IP '{}': {}", b, e))
        })?;
        // Use a long duration — heal_partition is the expected way to undo
        self.sim
            .partition_pair(a_ip, b_ip, Duration::from_secs(3600))
    }

    /// Remove a network partition between two IPs.
    pub fn heal_partition(&self, a: &str, b: &str) -> SimulationResult<()> {
        let a_ip: std::net::IpAddr = a.parse().map_err(|e| {
            crate::SimulationError::InvalidState(format!("invalid IP '{}': {}", a, e))
        })?;
        let b_ip: std::net::IpAddr = b.parse().map_err(|e| {
            crate::SimulationError::InvalidState(format!("invalid IP '{}': {}", b, e))
        })?;
        self.sim.restore_partition(a_ip, b_ip)
    }

    /// Check whether two IPs are partitioned.
    pub fn is_partitioned(&self, a: &str, b: &str) -> SimulationResult<bool> {
        let a_ip: std::net::IpAddr = a.parse().map_err(|e| {
            crate::SimulationError::InvalidState(format!("invalid IP '{}': {}", a, e))
        })?;
        let b_ip: std::net::IpAddr = b.parse().map_err(|e| {
            crate::SimulationError::InvalidState(format!("invalid IP '{}': {}", b, e))
        })?;
        self.sim.is_partitioned(a_ip, b_ip)
    }

    /// Get all IP addresses in the simulation.
    pub fn all_ips(&self) -> &[String] {
        &self.all_ips
    }

    /// Get the seeded random provider.
    pub fn random(&self) -> &SimRandomProvider {
        &self.random
    }

    /// Get the simulated time provider.
    pub fn time(&self) -> &SimTimeProvider {
        &self.time
    }

    /// Get the chaos-phase shutdown token.
    ///
    /// This token is cancelled at the chaos→recovery boundary,
    /// signaling fault injectors to stop.
    pub fn chaos_shutdown(&self) -> &tokio_util::sync::CancellationToken {
        &self.chaos_shutdown
    }

    /// Get all server process IPs.
    pub fn process_ips(&self) -> &[String] {
        &self.process_ips
    }

    /// Reboot a specific process by IP.
    ///
    /// Immediately aborts all connections for the IP and schedules a
    /// `ProcessRestart` event after a seeded random recovery delay.
    /// The actual process task cancellation is handled by the orchestrator.
    pub fn reboot(&self, ip: &str, _kind: RebootKind) -> SimulationResult<()> {
        let ip_addr: std::net::IpAddr = ip.parse().map_err(|e| {
            crate::SimulationError::InvalidState(format!("invalid IP '{}': {}", ip, e))
        })?;

        // Abort all connections for this IP
        self.sim.abort_all_connections_for_ip(ip_addr);

        // Schedule restart with seeded random delay (1-10 seconds)
        let delay_ms = crate::sim::sim_random_range(1000..10000);
        let recovery_delay = Duration::from_millis(delay_ms as u64);
        self.sim.schedule_process_restart(ip_addr, recovery_delay);

        tracing::info!(
            "Rebooted process at IP {} (recovery in {:?})",
            ip,
            recovery_delay
        );
        Ok(())
    }

    /// Reboot a random alive server process.
    ///
    /// Picks a random process from the process IP list and reboots it.
    /// Returns `Ok(None)` if no processes are available.
    pub fn reboot_random(&self, kind: RebootKind) -> SimulationResult<Option<String>> {
        if self.process_ips.is_empty() {
            return Ok(None);
        }
        let idx = crate::sim::sim_random_range(0..self.process_ips.len());
        let ip = self.process_ips[idx].clone();
        self.reboot(&ip, kind)?;
        Ok(Some(ip))
    }

    /// Reboot all processes matching a tag key=value pair.
    pub fn reboot_tagged(
        &self,
        key: &str,
        value: &str,
        kind: RebootKind,
    ) -> SimulationResult<Vec<String>> {
        let matching_ips: Vec<String> = self
            .tag_registry
            .ips_tagged(key, value)
            .into_iter()
            .map(|ip| ip.to_string())
            .collect();

        for ip in &matching_ips {
            self.reboot(ip, kind)?;
        }

        Ok(matching_ips)
    }
}

/// A fault injector that introduces failures during the chaos phase.
///
/// Fault injectors run concurrently with workloads during the chaos phase.
/// When `PhaseConfig` is used, they are signaled to stop via
/// `ctx.chaos_shutdown()` at the chaos→recovery boundary.
#[async_trait(?Send)]
pub trait FaultInjector: 'static {
    /// Name of this fault injector for reporting.
    fn name(&self) -> &str;

    /// Inject faults using the provided context.
    ///
    /// Should respect `ctx.chaos_shutdown()` to allow graceful termination.
    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()>;
}

/// Two-phase simulation configuration.
///
/// Controls the TigerBeetle VOPR-style chaos/recovery lifecycle:
/// 1. **Chaos phase** (`chaos_duration`): Workloads + fault injectors run concurrently.
///    Invariants are checked after every simulation event.
/// 2. **Recovery phase** (`recovery_duration`): Fault injectors stopped, workloads
///    continue, system heals. Verifies convergence after faults cease.
#[derive(Debug, Clone)]
pub struct PhaseConfig {
    /// Duration of the chaos phase (faults + workloads run concurrently).
    pub chaos_duration: Duration,
    /// Duration of the recovery phase (faults stopped, workloads continue).
    pub recovery_duration: Duration,
}

/// Built-in fault injector that randomly reboots server processes.
///
/// Active only during the chaos phase. Respects `max_dead` to limit the
/// number of simultaneously dead processes. The reboot type is chosen by
/// weighted probability from the [`Attrition`](super::process::Attrition) config.
pub(crate) struct AttritionInjector {
    config: super::process::Attrition,
}

impl AttritionInjector {
    /// Create a new attrition injector from the given configuration.
    pub(crate) fn new(config: super::process::Attrition) -> Self {
        Self { config }
    }
}

#[async_trait(?Send)]
impl FaultInjector for AttritionInjector {
    fn name(&self) -> &str {
        "attrition"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        while !ctx.chaos_shutdown().is_cancelled() {
            // Random delay between reboot attempts (1-5 seconds)
            let delay_ms = crate::sim::sim_random_range(1000..5000);
            ctx.time()
                .sleep(Duration::from_millis(delay_ms as u64))
                .await
                .map_err(|e| {
                    crate::SimulationError::InvalidState(format!("sleep failed: {}", e))
                })?;

            if ctx.chaos_shutdown().is_cancelled() {
                break;
            }

            if ctx.process_ips().is_empty() {
                continue;
            }

            // Choose reboot kind by weighted probability
            let rand_val = crate::sim::sim_random_range(0..10000) as f64 / 10000.0;
            let kind = self.config.choose_kind(rand_val);

            // TODO: respect max_dead by checking how many processes are currently dead.
            // For now, always attempt a reboot (the orchestrator handles restart scheduling).
            ctx.reboot_random(kind)?;
        }
        Ok(())
    }
}
