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

use crate::SimulationResult;
use crate::providers::{SimRandomProvider, SimTimeProvider};
use crate::sim::SimWorld;

/// Context for fault injectors — gives access to SimWorld fault injection methods.
///
/// Unlike `SimContext` (which workloads receive), `FaultContext` provides direct
/// access to network partitioning and other fault primitives that normal workloads
/// should not use.
pub struct FaultContext {
    sim: SimWorld,
    all_ips: Vec<String>,
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
