//! Fault injection for simulation chaos testing.
//!
//! [`FaultInjector`] defines fault injection strategies (partitions, connection drops, etc.)
//! that run during the chaos phase of a simulation. [`FaultContext`] provides access to
//! `SimWorld` fault injection primitives.
//!
//! When `chaos_duration` is configured on the builder, fault injectors run concurrently
//! with workloads. At the chaos boundary, `ctx.chaos_shutdown()` is cancelled and the
//! system settles before running workload checks.
//!
//! # Usage
//!
//! ```ignore
//! use moonpool_sim::{FaultInjector, FaultContext, SimulationResult};
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
use crate::{assert_reachable, assert_sometimes_each};

/// Process-related state for fault injection targeting.
pub struct ProcessInfo {
    /// Server process IP addresses.
    pub process_ips: Vec<String>,
    /// Tag registry mapping process IPs to their resolved tags.
    pub tag_registry: TagRegistry,
    /// Shared count of currently dead (killed but not yet restarted) processes.
    pub dead_count: std::rc::Rc<std::cell::Cell<usize>>,
}

/// Context for fault injectors — gives access to SimWorld fault injection methods.
///
/// Unlike `SimContext` (which workloads receive), `FaultContext` provides direct
/// access to network partitioning, reboot, and other fault primitives that normal
/// workloads should not use.
pub struct FaultContext {
    sim: SimWorld,
    all_ips: Vec<String>,
    process_info: ProcessInfo,
    random: SimRandomProvider,
    time: SimTimeProvider,
    chaos_shutdown: tokio_util::sync::CancellationToken,
}

impl FaultContext {
    /// Create a new fault context with process information.
    pub fn new(
        sim: SimWorld,
        all_ips: Vec<String>,
        process_info: ProcessInfo,
        random: SimRandomProvider,
        time: SimTimeProvider,
        chaos_shutdown: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            sim,
            all_ips,
            process_info,
            random,
            time,
            chaos_shutdown,
        }
    }

    /// Get the number of currently dead (killed but not yet restarted) processes.
    pub fn dead_count(&self) -> usize {
        self.process_info.dead_count.get()
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
        &self.process_info.process_ips
    }

    /// Reboot a specific process by IP.
    ///
    /// For [`RebootKind::Graceful`]: schedules a `ProcessGracefulShutdown` event.
    /// The orchestrator cancels the per-process shutdown token, giving the process
    /// a grace period to drain buffers and clean up. After the grace period,
    /// a force-kill aborts the task and connections, then schedules restart.
    ///
    /// For [`RebootKind::Crash`] and [`RebootKind::CrashAndWipe`]: immediately
    /// aborts all connections and schedules a `ProcessRestart` event.
    pub fn reboot(&self, ip: &str, kind: RebootKind) -> SimulationResult<()> {
        let recovery_range = 1000..10000;
        let grace_range = 2000..5000;
        self.reboot_with_delays(ip, kind, &recovery_range, &grace_range)
    }

    /// Reboot a process with custom delay ranges.
    ///
    /// Like [`reboot`](Self::reboot) but with configurable recovery delay and
    /// grace period ranges (in milliseconds). Used by [`AttritionInjector`] to
    /// pass through [`Attrition`](super::process::Attrition) configuration.
    pub fn reboot_with_delays(
        &self,
        ip: &str,
        kind: RebootKind,
        recovery_delay_range_ms: &std::ops::Range<usize>,
        grace_period_range_ms: &std::ops::Range<usize>,
    ) -> SimulationResult<()> {
        let ip_addr: std::net::IpAddr = ip.parse().map_err(|e| {
            crate::SimulationError::InvalidState(format!("invalid IP '{}': {}", ip, e))
        })?;

        match kind {
            RebootKind::Graceful => {
                assert_reachable!("reboot: graceful path");
                let grace_ms = crate::sim::sim_random_range(grace_period_range_ms.clone()) as u64;
                let recovery_ms =
                    crate::sim::sim_random_range(recovery_delay_range_ms.clone()) as u64;
                self.sim.schedule_event(
                    crate::sim::Event::ProcessGracefulShutdown {
                        ip: ip_addr,
                        grace_period_ms: grace_ms,
                        recovery_delay_ms: recovery_ms,
                    },
                    Duration::from_nanos(1),
                );
                tracing::info!(
                    "Initiated graceful reboot for process at IP {} (grace={}ms, recovery={}ms)",
                    ip,
                    grace_ms,
                    recovery_ms
                );
            }
            RebootKind::Crash | RebootKind::CrashAndWipe => {
                assert_reachable!("reboot: crash path");
                self.sim.abort_all_connections_for_ip(ip_addr);
                self.process_info
                    .dead_count
                    .set(self.process_info.dead_count.get() + 1);
                let delay_ms = crate::sim::sim_random_range(recovery_delay_range_ms.clone()) as u64;
                let recovery_delay = Duration::from_millis(delay_ms);
                self.sim.schedule_process_restart(ip_addr, recovery_delay);
                tracing::info!(
                    "Crashed process at IP {} (recovery in {:?})",
                    ip,
                    recovery_delay
                );
            }
        }

        Ok(())
    }

    /// Reboot a random alive server process.
    ///
    /// Picks a random process from the process IP list and reboots it.
    /// Returns `Ok(None)` if no processes are available.
    pub fn reboot_random(&self, kind: RebootKind) -> SimulationResult<Option<String>> {
        if self.process_info.process_ips.is_empty() {
            return Ok(None);
        }
        let idx = crate::sim::sim_random_range(0..self.process_info.process_ips.len());
        let ip = self.process_info.process_ips[idx].clone();
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
            .process_info
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
/// Fault injectors run concurrently with workloads when `chaos_duration` is set.
/// They are signaled to stop via `ctx.chaos_shutdown()` when the chaos duration
/// elapses. After all workloads complete, the system settles before checks run.
#[async_trait(?Send)]
pub trait FaultInjector: 'static {
    /// Name of this fault injector for reporting.
    fn name(&self) -> &str;

    /// Inject faults using the provided context.
    ///
    /// Should respect `ctx.chaos_shutdown()` to allow graceful termination.
    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()>;
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

            // Respect max_dead: skip this cycle if already at the limit
            if ctx.dead_count() >= self.config.max_dead {
                assert_reachable!("attrition: max_dead limit enforced");
                continue;
            }

            // Choose reboot kind by weighted probability
            let rand_val = crate::sim::sim_random_range(0..10000) as f64 / 10000.0;
            let kind = self.config.choose_kind(rand_val);
            assert_sometimes_each!("attrition_reboot_kind", [("kind", kind as i64)]);

            // Use configured delay ranges (or defaults)
            let recovery_range = self.config.recovery_delay_ms.clone().unwrap_or(1000..10000);
            let grace_range = self.config.grace_period_ms.clone().unwrap_or(2000..5000);

            if ctx.process_ips().is_empty() {
                continue;
            }
            let idx = crate::sim::sim_random_range(0..ctx.process_ips().len());
            let ip = ctx.process_ips()[idx].to_string();
            assert_sometimes_each!("attrition_process_targeted", [("process_idx", idx as i64)]);
            ctx.reboot_with_delays(&ip, kind, &recovery_range, &grace_range)?;
        }
        Ok(())
    }
}
