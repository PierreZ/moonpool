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
//! #[async_trait]
//! impl FaultInjector for RandomPartition {
//!     fn name(&self) -> &str { "random_partition" }
//!     async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
//!         let ips = ctx.process_ips();
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

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_core::TimeProvider;

use crate::SimulationResult;
use crate::locality::DomainLevel;
use crate::providers::{SimRandomProvider, SimTimeProvider};
use crate::runner::groups::GroupRegistry;
use crate::runner::locality::MachineRegistry;
use crate::runner::process::{AttritionScope, AttritionVictims, RebootKind};
use crate::runner::tags::TagRegistry;
use crate::sim::{ProcessKillKind, SimWorld};
use crate::{assert_always, assert_reachable, assert_sometimes_each};

/// Process-related state for fault injection targeting.
pub struct ProcessInfo {
    /// Server process IP addresses.
    pub process_ips: Vec<String>,
    /// Tag registry mapping process IPs to their resolved tags.
    pub tag_registry: TagRegistry,
    /// Machine registry mapping process IPs to their failure-domain locality.
    pub machine_registry: MachineRegistry,
    /// Group registry mapping process IPs to their `.processes()` / `.cluster()`
    /// registration.
    pub group_registry: GroupRegistry,
    /// The processes currently dead (killed but not yet restarted), shared
    /// with the process manager.
    pub dead: DeadSet,
}

/// The set of currently dead process IPs, shared between the process manager
/// (which records kills and restarts) and every fault context (which reads
/// it to budget kills). A set rather than a counter so a budget can be
/// scoped to one victim pool — "at most one dead acceptor" — and so a
/// second kill of an already-dead process cannot skew the count.
pub type DeadSet = Arc<Mutex<BTreeSet<std::net::IpAddr>>>;

/// Context for fault injectors — gives access to `SimWorld` fault injection methods.
///
/// Unlike `SimContext` (which workloads receive), `FaultContext` provides direct
/// access to network partitioning, reboot, and other fault primitives that normal
/// workloads should not use.
pub struct FaultContext {
    sim: SimWorld,
    process_info: ProcessInfo,
    random: SimRandomProvider,
    time: SimTimeProvider,
    state: crate::chaos::state_handle::StateHandle,
    chaos_shutdown: tokio_util::sync::CancellationToken,
}

impl FaultContext {
    /// Create a new fault context with process information.
    #[must_use]
    pub fn new(
        sim: SimWorld,
        process_info: ProcessInfo,
        random: SimRandomProvider,
        time: SimTimeProvider,
        state: crate::chaos::state_handle::StateHandle,
        chaos_shutdown: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            sim,
            process_info,
            random,
            time,
            state,
            chaos_shutdown,
        }
    }

    /// The per-iteration shared state handle, also visible to workloads and
    /// processes via `SimContext::state`.
    ///
    /// Lets a fault injector coordinate on deterministic workload milestones
    /// (e.g. "crash node X, wait until the workload has executed N operations,
    /// then restart X"). The values read here are per-iteration simulated
    /// state, so scripted sequences replay exactly from the root seed plus
    /// recipe.
    #[must_use]
    pub fn state(&self) -> &crate::chaos::state_handle::StateHandle {
        &self.state
    }

    /// Get the number of currently dead (killed but not yet restarted) processes.
    ///
    /// # Panics
    ///
    /// Panics if the shared dead set's lock is poisoned (a prior task panicked).
    #[must_use]
    pub fn dead_count(&self) -> usize {
        self.process_info
            .dead
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .len()
    }

    /// Whether the process at `ip` is currently dead (killed but not yet
    /// restarted). An unparsable IP is never dead.
    ///
    /// # Panics
    ///
    /// Panics if the shared dead set's lock is poisoned (a prior task panicked).
    #[must_use]
    pub fn is_dead(&self, ip: &str) -> bool {
        ip.parse::<std::net::IpAddr>().is_ok_and(|ip| {
            self.process_info
                .dead
                .lock()
                .expect("Mutex poisoned: prior task panicked")
                .contains(&ip)
        })
    }

    /// Record `ip` as dead: the kill has been scheduled, so it counts against
    /// every injector's budget from this instant, not from when the event runs.
    fn mark_dead(&self, ip: std::net::IpAddr) {
        self.process_info
            .dead
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .insert(ip);
    }

    /// Create a bidirectional network partition between two IPs.
    ///
    /// The partition persists until [`heal_partition`](Self::heal_partition) is called.
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails.
    pub fn partition(&self, a: &str, b: &str) -> SimulationResult<()> {
        let a_ip: std::net::IpAddr = a
            .parse()
            .map_err(|e| crate::SimulationError::InvalidState(format!("invalid IP '{a}': {e}")))?;
        let b_ip: std::net::IpAddr = b
            .parse()
            .map_err(|e| crate::SimulationError::InvalidState(format!("invalid IP '{b}': {e}")))?;
        // Use a long duration — heal_partition is the expected way to undo
        self.sim.partition_pair(a_ip, b_ip, Duration::from_hours(1));
        self.sim.partition_pair(b_ip, a_ip, Duration::from_hours(1));
        Ok(())
    }

    /// Remove a network partition between two IPs.
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails.
    pub fn heal_partition(&self, a: &str, b: &str) -> SimulationResult<()> {
        let a_ip: std::net::IpAddr = a
            .parse()
            .map_err(|e| crate::SimulationError::InvalidState(format!("invalid IP '{a}': {e}")))?;
        let b_ip: std::net::IpAddr = b
            .parse()
            .map_err(|e| crate::SimulationError::InvalidState(format!("invalid IP '{b}': {e}")))?;
        self.sim.restore_partition(a_ip, b_ip);
        Ok(())
    }

    /// Check whether two IPs are partitioned.
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails.
    pub fn is_partitioned(&self, a: &str, b: &str) -> SimulationResult<bool> {
        let a_ip: std::net::IpAddr = a
            .parse()
            .map_err(|e| crate::SimulationError::InvalidState(format!("invalid IP '{a}': {e}")))?;
        let b_ip: std::net::IpAddr = b
            .parse()
            .map_err(|e| crate::SimulationError::InvalidState(format!("invalid IP '{b}': {e}")))?;
        Ok(self.sim.is_partitioned(a_ip, b_ip))
    }

    /// Get the seeded random provider.
    #[must_use]
    pub fn random(&self) -> &SimRandomProvider {
        &self.random
    }

    /// Get the simulated time provider.
    #[must_use]
    pub fn time(&self) -> &SimTimeProvider {
        &self.time
    }

    /// Get the chaos-phase shutdown token.
    ///
    /// This token is cancelled at the chaos→recovery boundary,
    /// signaling fault injectors to stop.
    #[must_use]
    pub fn chaos_shutdown(&self) -> &tokio_util::sync::CancellationToken {
        &self.chaos_shutdown
    }

    /// Get all server process IPs.
    #[must_use]
    pub fn process_ips(&self) -> &[String] {
        &self.process_info.process_ips
    }

    /// The group registry, for process-group queries during fault injection.
    #[must_use]
    pub fn group_registry(&self) -> &GroupRegistry {
        &self.process_info.group_registry
    }

    /// Get the IPs of every process in a group (a `.processes()` /
    /// `.cluster()` registration, named after its process type), ascending.
    #[must_use]
    pub fn ips_in_group(&self, group: &str) -> Vec<String> {
        self.process_info
            .group_registry
            .ips_in_group(group)
            .into_iter()
            .map(|ip| ip.to_string())
            .collect()
    }

    /// Reboot a specific process by IP.
    ///
    /// For [`RebootKind::Graceful`]: schedules a `ProcessGracefulShutdown` event.
    /// The orchestrator cancels the per-process shutdown token, giving the process
    /// a grace period to drain buffers and clean up. After the grace period,
    /// a force-kill aborts the task and connections, then schedules restart.
    ///
    /// For [`RebootKind::Crash`] and [`RebootKind::CrashAndWipe`]: schedules a
    /// `ProcessForceKill` event at the crash instant. The orchestrator aborts
    /// the process task before aborting its connections and crashing (or
    /// wiping) its storage, then schedules the restart. The process runs no
    /// further application work during the recovery delay.
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails or the operation is rejected by the simulator.
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
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails or the operation is rejected by the simulator.
    pub fn reboot_with_delays(
        &self,
        ip: &str,
        kind: RebootKind,
        recovery_delay_range_ms: &std::ops::Range<usize>,
        grace_period_range_ms: &std::ops::Range<usize>,
    ) -> SimulationResult<()> {
        let ip_addr: std::net::IpAddr = ip
            .parse()
            .map_err(|e| crate::SimulationError::InvalidState(format!("invalid IP '{ip}': {e}")))?;

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
                self.mark_dead(ip_addr);
                let delay_ms = crate::sim::sim_random_range(recovery_delay_range_ms.clone()) as u64;
                let cause = if kind == RebootKind::CrashAndWipe {
                    ProcessKillKind::CrashAndWipe
                } else {
                    ProcessKillKind::Crash
                };
                // A crash is a force-kill, not just a restart: the orchestrator
                // owns the process handles, so route through ProcessForceKill so
                // the task dies now instead of surviving the recovery delay.
                self.sim.schedule_event(
                    crate::sim::Event::ProcessForceKill {
                        ip: ip_addr,
                        recovery_delay_ms: Some(delay_ms),
                        cause,
                    },
                    Duration::from_nanos(1),
                );
                tracing::info!("Crashed process at IP {} (recovery in {}ms)", ip, delay_ms);
            }
        }

        Ok(())
    }

    /// Crash a process **without scheduling a restart** (hold-down).
    ///
    /// Unlike [`reboot`](Self::reboot) with [`RebootKind::Crash`] — which
    /// combines the crash with a timer-based restart — this schedules only the
    /// force-kill: the process task is aborted, its connections die, and its
    /// unsynced storage state is lost, but no recovery timer is armed. The
    /// process stays down until an explicit [`restart`](Self::restart), so a
    /// scripted injector can hold a node down across a deterministic workload
    /// milestone (observed via [`state`](Self::state)).
    ///
    /// The crashed process counts toward [`dead_count`](Self::dead_count)
    /// until restarted.
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails.
    pub fn crash(&self, ip: &str) -> SimulationResult<()> {
        let ip_addr: std::net::IpAddr = ip
            .parse()
            .map_err(|e| crate::SimulationError::InvalidState(format!("invalid IP '{ip}': {e}")))?;
        assert_reachable!("crash: hold-down path");
        self.mark_dead(ip_addr);
        self.sim.schedule_event(
            crate::sim::Event::ProcessForceKill {
                ip: ip_addr,
                recovery_delay_ms: None,
                cause: ProcessKillKind::Crash,
            },
            Duration::from_nanos(1),
        );
        tracing::info!("Crashed process at IP {} (held down until restart)", ip);
        Ok(())
    }

    /// Explicitly restart a process, typically one held down by
    /// [`crash`](Self::crash).
    ///
    /// Schedules a `ProcessRestart` event: the orchestrator boots a fresh
    /// instance from the process factory and the process leaves
    /// [`dead_count`](Self::dead_count). Restarting a process that is still
    /// running aborts it first and boots a fresh instance (a zero-downtime
    /// reboot).
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails.
    pub fn restart(&self, ip: &str) -> SimulationResult<()> {
        let ip_addr: std::net::IpAddr = ip
            .parse()
            .map_err(|e| crate::SimulationError::InvalidState(format!("invalid IP '{ip}': {e}")))?;
        assert_reachable!("restart: explicit restart path");
        self.sim.schedule_event(
            crate::sim::Event::ProcessRestart { ip: ip_addr },
            Duration::from_nanos(1),
        );
        tracing::info!("Explicitly restarting process at IP {}", ip);
        Ok(())
    }

    /// Reboot a random alive server process.
    ///
    /// Picks a random process from the process IP list and reboots it.
    /// Returns `Ok(None)` if no processes are available.
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails or the operation is rejected by the simulator.
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
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails or the operation is rejected by the simulator.
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

    /// The machine registry, for failure-domain queries during fault injection.
    #[must_use]
    pub fn machine_registry(&self) -> &MachineRegistry {
        &self.process_info.machine_registry
    }

    /// Reboot every process collocated on a single machine — modeling correlated
    /// (shared-fate) failure.
    ///
    /// Returns the IPs that were rebooted (empty if the machine is unknown).
    ///
    /// # Errors
    ///
    /// Returns an error if the operation is rejected by the simulator.
    pub fn reboot_machine(
        &self,
        machine_id: &str,
        kind: RebootKind,
    ) -> SimulationResult<Vec<String>> {
        self.reboot_domain(DomainLevel::Machine, machine_id, kind)
    }

    /// Reboot every process in a failure domain (`level` + `id`) together.
    ///
    /// Returns the IPs that were rebooted (empty if the domain is unknown).
    ///
    /// # Errors
    ///
    /// Returns an error if the operation is rejected by the simulator.
    pub fn reboot_domain(
        &self,
        level: DomainLevel,
        id: &str,
        kind: RebootKind,
    ) -> SimulationResult<Vec<String>> {
        let ips: Vec<String> = self
            .process_info
            .machine_registry
            .ips_in_domain(level, id)
            .into_iter()
            .map(|ip| ip.to_string())
            .collect();

        for ip in &ips {
            self.reboot(ip, kind)?;
        }

        Ok(ips)
    }
}

/// A fault injector that introduces failures during the chaos phase.
///
/// Fault injectors run concurrently with workloads when `chaos_duration` is set.
/// They are signaled to stop via `ctx.chaos_shutdown()` when the chaos duration
/// elapses. After all workloads complete, the system settles before checks run.
#[async_trait]
pub trait FaultInjector: Send + Sync + 'static {
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
/// number of simultaneously dead processes **among its victim pool**: with
/// [`AttritionVictims::Any`] that is every process, so the budget is
/// cluster-wide; with a group or tag filter it is that pool alone, so two
/// injectors with different pools budget independently. The reboot type is
/// chosen by weighted probability from the
/// [`Attrition`](super::process::Attrition) config.
pub(crate) struct AttritionInjector {
    config: super::process::Attrition,
    /// `attrition`, or `attrition[group=…]` / `attrition[tag=k=v]` for a
    /// filtered injector, so a report tells the campaign's injectors apart.
    name: String,
}

impl AttritionInjector {
    /// Create a new attrition injector from the given configuration.
    pub(crate) fn new(config: super::process::Attrition) -> Self {
        let name = match &config.victims {
            AttritionVictims::Any => "attrition".to_string(),
            AttritionVictims::Group(group) => format!("attrition[group={group}]"),
            AttritionVictims::Tagged { key, value } => format!("attrition[tag={key}={value}]"),
        };
        Self { config, name }
    }

    /// How many of `ips` are currently dead: the injector's `max_dead` budget
    /// is spent against its own victim pool.
    fn dead_among(ctx: &FaultContext, ips: &[&String]) -> usize {
        ips.iter().filter(|ip| ctx.is_dead(ip)).count()
    }

    /// Draw a reboot kind by weighted probability and record coverage.
    fn choose_kind(&self) -> RebootKind {
        let rand_val = f64::from(crate::sim::sim_random_range(0..10000)) / 10000.0;
        let kind = self.config.choose_kind(rand_val);
        assert_sometimes_each!("attrition_reboot_kind", [("kind", kind as i64)]);
        kind
    }

    /// Configured recovery / grace-period delay ranges, with defaults.
    fn delay_ranges(&self) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
        (
            self.config.recovery_delay_ms.clone().unwrap_or(1000..10000),
            self.config.grace_period_ms.clone().unwrap_or(2000..5000),
        )
    }

    /// Whether the victim filter admits `ip`; an unparsable IP is never
    /// eligible.
    fn admits(&self, ctx: &FaultContext, ip: &str) -> bool {
        ip.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| self.config.victims.admits(ip, &ctx.process_info))
    }

    /// The processes among `ips` the victim filter admits.
    ///
    /// With [`AttritionVictims::Any`] this is `ips` itself, so the draw over
    /// it is the same draw an unfiltered campaign makes: the filter consumes
    /// no randomness and, when it excludes nothing, shifts no seed.
    fn eligible<'i>(&self, ctx: &FaultContext, ips: &'i [String]) -> Vec<&'i String> {
        ips.iter().filter(|ip| self.admits(ctx, ip)).collect()
    }

    /// Record and guard the filter contract for one chosen victim.
    fn check_victim(&self, ctx: &FaultContext, ip: &str) {
        assert_always!(
            self.admits(ctx, ip),
            "attrition: victim admitted by the victim filter",
            { "ip" => ip, "victims" => format!("{:?}", self.config.victims) }
        );
        if self.config.victims != AttritionVictims::Any {
            assert_reachable!("attrition: victim filter narrowed the pool");
        }
    }

    /// Reboot a single random eligible process, respecting the `max_dead`
    /// budget over the eligible pool.
    fn inject_process(&self, ctx: &FaultContext) -> SimulationResult<()> {
        let eligible = self.eligible(ctx, ctx.process_ips());
        if Self::dead_among(ctx, &eligible) >= self.config.max_dead {
            assert_reachable!("attrition: max_dead limit enforced");
            return Ok(());
        }
        if eligible.is_empty() {
            assert_reachable!("attrition: no eligible victim");
            return Ok(());
        }
        let kind = self.choose_kind();
        let (recovery_range, grace_range) = self.delay_ranges();
        let idx = crate::sim::sim_random_range(0..eligible.len());
        let ip = eligible[idx].clone();
        assert_sometimes_each!(
            "attrition_process_targeted",
            [("process_idx", i64::try_from(idx).unwrap_or(i64::MAX))]
        );
        self.check_victim(ctx, &ip);
        ctx.reboot_with_delays(&ip, kind, &recovery_range, &grace_range)
    }

    /// Reboot every eligible process in a random failure domain *together*,
    /// only if the group fits within the `max_dead` budget over the eligible
    /// pool. A no-op when no locality topology is configured (`domains`
    /// empty). Domains holding no eligible process are never drawn, so the
    /// victim filter cannot make a round silently pick an empty group.
    fn inject_domain(
        &self,
        ctx: &FaultContext,
        level: DomainLevel,
        domains: &[String],
    ) -> SimulationResult<()> {
        let domain_ips = |domain: &String| -> Vec<String> {
            let all: Vec<String> = ctx
                .machine_registry()
                .ips_in_domain(level, domain)
                .into_iter()
                .map(|ip| ip.to_string())
                .collect();
            self.eligible(ctx, &all).into_iter().cloned().collect()
        };
        let domains: Vec<&String> = domains
            .iter()
            .filter(|domain| !domain_ips(domain).is_empty())
            .collect();
        if domains.is_empty() {
            return Ok(());
        }
        let di = crate::sim::sim_random_range(0..domains.len());
        let ips = domain_ips(domains[di]);
        // Whole-group gate: reboot the group atomically only if all of its
        // processes fit within the remaining budget of the eligible pool.
        let eligible = self.eligible(ctx, ctx.process_ips());
        if Self::dead_among(ctx, &eligible) + ips.len() > self.config.max_dead {
            assert_reachable!("attrition: max_dead limit enforced (group)");
            return Ok(());
        }
        let kind = self.choose_kind();
        let (recovery_range, grace_range) = self.delay_ranges();
        assert_sometimes_each!(
            "attrition_domain_targeted",
            [("group_size", i64::try_from(ips.len()).unwrap_or(i64::MAX))]
        );
        for ip in &ips {
            self.check_victim(ctx, ip);
            ctx.reboot_with_delays(ip, kind, &recovery_range, &grace_range)?;
        }
        Ok(())
    }
}

#[async_trait]
impl FaultInjector for AttritionInjector {
    fn name(&self) -> &str {
        &self.name
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        while !ctx.chaos_shutdown().is_cancelled() {
            // Random delay between reboot attempts (1-5 seconds)
            let delay_ms = crate::sim::sim_random_range(1000..5000);
            ctx.time()
                .sleep(Duration::from_millis(
                    u64::try_from(delay_ms).expect("delay_ms is non-negative"),
                ))
                .await
                .map_err(|e| crate::SimulationError::InvalidState(format!("sleep failed: {e}")))?;

            if ctx.chaos_shutdown().is_cancelled() {
                break;
            }

            if ctx.process_ips().is_empty() {
                continue;
            }

            match self.config.scope {
                AttritionScope::PerProcess => self.inject_process(ctx)?,
                AttritionScope::PerMachine => {
                    let machines = ctx.machine_registry().all_machines();
                    self.inject_domain(ctx, DomainLevel::Machine, &machines)?;
                }
                AttritionScope::PerZone => {
                    let zones = ctx.machine_registry().all_zones();
                    self.inject_domain(ctx, DomainLevel::Zone, &zones)?;
                }
                AttritionScope::PerDatacenter => {
                    let datacenters = ctx.machine_registry().all_datacenters();
                    self.inject_domain(ctx, DomainLevel::Datacenter, &datacenters)?;
                }
            }
        }
        Ok(())
    }
}
