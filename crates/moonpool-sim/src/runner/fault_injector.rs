//! Custom fault injection: scripted faults the built-in chaos surfaces do
//! not cover.
//!
//! A [`FaultInjector`] runs beside the workloads during the chaos window
//! ([`SimulationBuilder::chaos_duration`](crate::SimulationBuilder::chaos_duration)).
//! Its [`FaultContext`] gives it the fault primitives a workload must not
//! have:
//!
//! | Need | Methods |
//! |------|---------|
//! | Partition the network | [`partition`](FaultContext::partition), [`heal_partition`](FaultContext::heal_partition), [`is_partitioned`](FaultContext::is_partitioned) |
//! | Kill and restart processes | [`reboot`](FaultContext::reboot), [`reboot_with_delays`](FaultContext::reboot_with_delays), [`crash`](FaultContext::crash), [`crash_for`](FaultContext::crash_for), [`restart`](FaultContext::restart), [`reboot_random`](FaultContext::reboot_random), [`reboot_tagged`](FaultContext::reboot_tagged), [`reboot_machine`](FaultContext::reboot_machine), [`reboot_domain`](FaultContext::reboot_domain) |
//! | Damage storage | [`corrupt_bytes`](FaultContext::corrupt_bytes), [`fail_file_with_eio`](FaultContext::fail_file_with_eio), [`clear_file_eio`](FaultContext::clear_file_eio), [`durable_bytes`](FaultContext::durable_bytes) |
//! | Slow a machine | [`set_slowness`](FaultContext::set_slowness), [`slowness`](FaultContext::slowness), [`cpu_model`](FaultContext::cpu_model) |
//! | Find targets | [`process_ips`](FaultContext::process_ips), [`ips_in_group`](FaultContext::ips_in_group), [`machine_registry`](FaultContext::machine_registry), [`is_dead`](FaultContext::is_dead), [`dead_count`](FaultContext::dead_count) |
//!
//! Register an injector with
//! [`SimulationBuilder::fault_factory`](crate::SimulationBuilder::fault_factory).
//! The factory builds a fresh injector for every seed and every explored
//! timeline, so a scripted sequence replays exactly. At the end of the
//! window, [`chaos_shutdown`](FaultContext::chaos_shutdown) is cancelled:
//! the injector must return.
//!
//! # Usage
//!
//! ```rust
//! use std::time::Duration;
//!
//! use async_trait::async_trait;
//! use moonpool_sim::{
//!     FaultContext, FaultInjector, RandomProvider, SimulationBuilder, SimulationResult,
//!     TimeProvider,
//! };
//!
//! /// Cuts the first two processes apart for five seconds, now and then.
//! struct RandomPartition {
//!     probability: f64,
//! }
//!
//! #[async_trait]
//! impl FaultInjector for RandomPartition {
//!     fn name(&self) -> &str {
//!         "random_partition"
//!     }
//!
//!     async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
//!         let ips = ctx.process_ips();
//!         while !ctx.chaos_shutdown().is_cancelled() && ips.len() >= 2 {
//!             if ctx.random().random_bool(self.probability) {
//!                 ctx.partition(&ips[0], &ips[1])?;
//!                 let _ = ctx.time().sleep(Duration::from_secs(5)).await;
//!                 ctx.heal_partition(&ips[0], &ips[1])?;
//!             }
//!             let _ = ctx.time().sleep(Duration::from_secs(1)).await;
//!         }
//!         Ok(())
//!     }
//! }
//!
//! let _builder = SimulationBuilder::new()
//!     .fault_factory(|| Box::new(RandomPartition { probability: 0.1 }))
//!     // Required: injectors run only inside the chaos window.
//!     .chaos_duration(Duration::from_secs(60));
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
use crate::runner::membership::Membership;
use crate::runner::process::{AttritionScope, AttritionVictims, RebootKind};
use crate::runner::shard::{Downgrade, Requirements, ShardBook, Shards};
use crate::runner::wipe::WipeLedger;
use crate::sim::{ProcessKillKind, SimWorld};
use crate::{assert_always, assert_reachable, assert_sometimes_each};

/// Process-related state for fault injection targeting.
pub struct ProcessInfo {
    /// The run's live membership: groups, tags and failure domains of every
    /// process now, spawned and retired ones included.
    pub membership: Membership,
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

/// Context for fault injectors: the fault primitives of the
/// [`SimWorld`].
///
/// Unlike [`SimContext`](crate::SimContext) (which processes and workloads
/// receive), `FaultContext` gives direct access to network partitions,
/// reboots, storage damage and machine slowness. It also shares the run's
/// [`state`](Self::state), so an injector can coordinate with workloads.
/// See the [module documentation](self) for the full list.
pub struct FaultContext {
    sim: SimWorld,
    process_info: ProcessInfo,
    random: SimRandomProvider,
    time: SimTimeProvider,
    state: crate::chaos::state_handle::StateHandle,
    chaos_shutdown: tokio_util::sync::CancellationToken,
}

/// Parse a process IP as the simulator addresses it.
fn parse_ip(ip: &str) -> SimulationResult<std::net::IpAddr> {
    ip.parse()
        .map_err(|e| crate::SimulationError::InvalidState(format!("invalid IP '{ip}': {e}")))
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
        let a_ip = parse_ip(a)?;
        let b_ip = parse_ip(b)?;
        if let Some((book, requirements)) = self.gate_requirements() {
            let dead = self.dead_set();
            let cut = self.cut();
            if book.config().judges_progress()
                && !super::shard::pure(|| {
                    super::shard::split_ok(&requirements, &dead, &[a_ip], &[b_ip], &cut)
                })
            {
                assert_reachable!("shard gate: a scripted partition is refused");
                book.count(Downgrade::PartitionRefused);
                return Ok(());
            }
            book.note_fault(a_ip);
            book.note_fault(b_ip);
        }
        // Use a long duration — heal_partition is the expected way to undo
        self.sim.partition_pair(a_ip, b_ip, Duration::from_hours(1));
        self.sim.partition_pair(b_ip, a_ip, Duration::from_hours(1));
        Ok(())
    }

    /// The durable bytes of the file at `path` on `ip`'s disk, to read a
    /// format's layout before aiming a targeted injection at it (see
    /// [`SimWorld::process_file_durable_bytes`]). Takes no simulated time and
    /// draws no randomness.
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails or `ip` has no file at `path`.
    pub fn durable_bytes(&self, ip: &str, path: &str) -> SimulationResult<Vec<u8>> {
        let ip = parse_ip(ip)?;
        self.sim
            .process_file_durable_bytes(ip, path)
            .map_err(|error| std::io::Error::from(error).into())
    }

    /// Durably mutate exactly `bytes` of the file at `path` on `ip`'s disk,
    /// leaving every other process's file at that path alone (see
    /// [`SimWorld::corrupt_process_file_bytes`]). No randomness is drawn,
    /// and a later crash does not take the change for a lost synced write.
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails, `ip` has no file at `path`, or
    /// `bytes` reach past its durable end.
    pub fn corrupt_bytes(
        &self,
        ip: &str,
        path: &str,
        bytes: std::ops::Range<u64>,
    ) -> SimulationResult<()> {
        let ip = parse_ip(ip)?;
        if let Some(book) = ShardBook::of(&self.state)
            && !super::shard::pure(|| self.sim.shard_damage_allowed(ip, path, &bytes))
        {
            assert_reachable!("shard gate: scripted damage is refused");
            book.count(Downgrade::DamageRefused);
            return Ok(());
        }
        self.sim
            .corrupt_process_file_bytes(ip, path, bytes)
            .map_err(|error| std::io::Error::from(error).into())
    }

    /// Make reads and/or writes touching `sectors` of the file at `path` on
    /// `ip`'s disk fail with an I/O error, until
    /// [`clear_file_eio`](Self::clear_file_eio). Other processes' files at
    /// that path are unaffected.
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails or `ip` has no file at `path`.
    pub fn fail_file_with_eio(
        &self,
        ip: &str,
        path: &str,
        sectors: std::ops::Range<u64>,
        target: crate::storage::EioTarget,
    ) -> SimulationResult<()> {
        let ip = parse_ip(ip)?;
        self.sim
            .fail_process_file_with_eio(ip, path, sectors, target)
            .map_err(|error| std::io::Error::from(error).into())
    }

    /// Clear the targeted EIO injections on the file at `path` on `ip`'s
    /// disk.
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails or `ip` has no file at `path`.
    pub fn clear_file_eio(
        &self,
        ip: &str,
        path: &str,
        target: crate::storage::EioTarget,
    ) -> SimulationResult<()> {
        let ip = parse_ip(ip)?;
        self.sim
            .clear_process_file_eio(ip, path, target)
            .map_err(|error| std::io::Error::from(error).into())
    }

    /// Remove a network partition between two IPs.
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails.
    pub fn heal_partition(&self, a: &str, b: &str) -> SimulationResult<()> {
        let a_ip = parse_ip(a)?;
        let b_ip = parse_ip(b)?;
        self.sim.restore_partition(a_ip, b_ip);
        Ok(())
    }

    /// Check whether two IPs are partitioned.
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails.
    pub fn is_partitioned(&self, a: &str, b: &str) -> SimulationResult<bool> {
        let a_ip = parse_ip(a)?;
        let b_ip = parse_ip(b)?;
        Ok(self.sim.is_partitioned(a_ip, b_ip))
    }

    /// Make the machine at `ip` slow, or healthy again with
    /// [`Slowness::HEALTHY`](crate::Slowness::HEALTHY): a gray failure, the
    /// machine stays alive (see [`SimWorld::set_slowness`]).
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails.
    pub fn set_slowness(&self, ip: &str, slowness: crate::Slowness) -> SimulationResult<()> {
        self.sim.set_slowness(parse_ip(ip)?, slowness);
        Ok(())
    }

    /// How slow the machine at `ip` is now (see [`SimWorld::slowness`]).
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails.
    pub fn slowness(&self, ip: &str) -> SimulationResult<crate::Slowness> {
        Ok(self.sim.slowness(parse_ip(ip)?))
    }

    /// Give the disk of the process at `ip` a size limit, or lift it with
    /// `None`: it fills up and refuses growth with `ENOSPC` (see
    /// [`SimWorld::set_disk_capacity`]).
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails.
    pub fn set_disk_capacity(
        &self,
        ip: &str,
        capacity: Option<crate::DiskCapacity>,
    ) -> SimulationResult<()> {
        self.sim.set_disk_capacity(parse_ip(ip)?, capacity);
        Ok(())
    }

    /// The space on the disk of the process at `ip`, when it has a size
    /// limit (see [`SimWorld::disk_space`]).
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails.
    pub fn disk_space(&self, ip: &str) -> SimulationResult<Option<crate::DiskSpace>> {
        Ok(self.sim.disk_space(parse_ip(ip)?))
    }

    /// The seed's CPU model, if it is on (see
    /// [`SimWorld::set_cpu_model`]).
    #[must_use]
    pub fn cpu_model(&self) -> Option<crate::CpuModel> {
        self.sim.cpu_model()
    }

    /// The process metadata the built-in injectors filter victims with.
    pub(crate) fn process_info(&self) -> &ProcessInfo {
        &self.process_info
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

    /// Get every live server process IP (booted or spawned, not retired),
    /// read now.
    #[must_use]
    pub fn process_ips(&self) -> Vec<String> {
        self.process_info.membership.process_ips()
    }

    /// The run's live membership.
    #[must_use]
    pub fn membership(&self) -> &Membership {
        &self.process_info.membership
    }

    /// The membership version: +1 on every spawned process that boots and
    /// every retire.
    #[must_use]
    pub fn membership_version(&self) -> u64 {
        self.process_info.membership.version()
    }

    /// A copy of the live group registry, for process-group queries during
    /// fault injection.
    #[must_use]
    pub fn group_registry(&self) -> GroupRegistry {
        self.process_info.membership.group_registry()
    }

    /// Get the IPs of every process in a group (a `.processes()` /
    /// `.cluster()` registration, named after its process type), ascending.
    #[must_use]
    pub fn ips_in_group(&self, group: &str) -> Vec<String> {
        self.process_info
            .membership
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
    /// Either way the process counts as dead from this call on (so a second
    /// reboot in the same tick sees it against `max_dead`), and a process that
    /// is already dead — its kill scheduled, its restart not yet run — is left
    /// alone: rebooting it again would schedule a second restart that kills
    /// and replaces the incarnation the first one boots. A held-down process
    /// ([`crash`](Self::crash)) comes back through [`restart`](Self::restart).
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
    /// grace period ranges (in milliseconds). Used by the built-in attrition injector to
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
        let ip_addr = parse_ip(ip)?;
        if self.retired(ip_addr) {
            return Ok(());
        }
        if self.is_dead(ip) {
            assert_reachable!("reboot: target already dead, skipped");
            tracing::debug!("Skipped reboot of already-dead process at IP {}", ip);
            return Ok(());
        }
        let Some(kind) = self.gate_reboot(ip_addr, kind) else {
            return self.restart(ip);
        };

        match kind {
            RebootKind::Graceful => {
                assert_reachable!("reboot: graceful path");
                // Dead from the decision, like the crash path: the shutdown
                // event runs a tick later, and a budget checked in between
                // must already count this process.
                self.mark_dead(ip_addr);
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
            RebootKind::Crash | RebootKind::CrashAndWipe | RebootKind::KillKeepCache => {
                assert_reachable!("reboot: crash path");
                self.mark_dead(ip_addr);
                if kind == RebootKind::CrashAndWipe {
                    // Unhealed from the decision, like dead: a wipe budget
                    // checked before the kill runs must already count it.
                    if let Some(ledger) = WipeLedger::of(&self.state) {
                        ledger.record_wipe(ip_addr);
                    }
                    if let Some(book) = ShardBook::of(&self.state) {
                        book.note_wipe(ip_addr);
                    }
                }
                let delay_ms = crate::sim::sim_random_range(recovery_delay_range_ms.clone()) as u64;
                let cause = match kind {
                    RebootKind::CrashAndWipe => ProcessKillKind::CrashAndWipe,
                    RebootKind::KillKeepCache => ProcessKillKind::KillKeepCache,
                    _ => ProcessKillKind::Crash,
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
        let ip_addr = parse_ip(ip)?;
        if self.retired(ip_addr) {
            return Ok(());
        }
        let Some(kind) = self.gate_reboot(ip_addr, RebootKind::Crash) else {
            return self.restart(ip);
        };
        assert_reachable!("crash: hold-down path");
        self.mark_dead(ip_addr);
        self.sim.schedule_event(
            crate::sim::Event::ProcessForceKill {
                ip: ip_addr,
                recovery_delay_ms: None,
                cause: kill_cause(kind),
            },
            Duration::from_nanos(1),
        );
        tracing::info!("Crashed process at IP {} (held down until restart)", ip);
        Ok(())
    }

    /// Crash a process now and restart it after exactly `down`: no draw is
    /// made, and the restart is a scheduled event, so it runs even once the
    /// chaos window has closed and this injector is gone. A process already
    /// dead is left alone (see [`reboot`](Self::reboot)).
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails.
    pub fn crash_for(&self, ip: &str, down: Duration) -> SimulationResult<()> {
        self.crash_for_gated(ip, down).map(|_| ())
    }

    /// [`crash_for`](Self::crash_for), answering whether the crash was
    /// scheduled: the shard gate spares a victim whose loss would stop a
    /// shard's progress.
    pub(crate) fn crash_for_gated(&self, ip: &str, down: Duration) -> SimulationResult<bool> {
        let ip_addr = parse_ip(ip)?;
        if self.retired(ip_addr) {
            return Ok(false);
        }
        if self.is_dead(ip) {
            assert_reachable!("crash_for: target already dead, skipped");
            return Ok(false);
        }
        let Some(kind) = self.gate_crash(ip_addr, RebootKind::Crash) else {
            assert_reachable!("shard gate: a timed crash spares its victim");
            if let Some(book) = ShardBook::of(&self.state) {
                book.count(Downgrade::CrashSpared);
            }
            return Ok(false);
        };
        self.mark_dead(ip_addr);
        self.sim.schedule_event(
            crate::sim::Event::ProcessForceKill {
                ip: ip_addr,
                recovery_delay_ms: Some(u64::try_from(down.as_millis()).unwrap_or(u64::MAX)),
                cause: kill_cause(kind),
            },
            Duration::from_nanos(1),
        );
        tracing::info!("Crashed process at IP {} (restart in {:?})", ip, down);
        Ok(true)
    }

    /// The run's shard book and the shards its gate covers, if any.
    fn gate_requirements(&self) -> Option<(ShardBook, Requirements)> {
        let book = ShardBook::of(&self.state)?;
        let requirements = book.requirements();
        (!requirements.is_empty()).then_some((book, requirements))
    }

    /// The processes dead now.
    fn dead_set(&self) -> std::collections::BTreeSet<std::net::IpAddr> {
        self.process_info
            .dead
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .clone()
    }

    /// Whether `a` cannot reach `b` now, in either direction.
    fn cut(&self) -> impl Fn(std::net::IpAddr, std::net::IpAddr) -> bool + '_ {
        |a, b| self.sim.is_partitioned(a, b) || self.sim.is_partitioned(b, a)
    }

    /// The replicas whose copies count as lost now: wiped and not healed,
    /// or on a failed disk.
    fn not_intact(
        &self,
        book: &ShardBook,
        requirements: &Requirements,
    ) -> std::collections::BTreeSet<std::net::IpAddr> {
        let mut out = book.wiped();
        for (_, replications) in requirements {
            for replication in replications {
                for replica in replication
                    .replicas
                    .iter()
                    .chain(&replication.durable.replicas())
                {
                    if let Some(ip) = super::shard::replica_ip(replica)
                        && self.sim.is_disk_failed(ip)
                    {
                        out.insert(ip);
                    }
                }
            }
        }
        out
    }

    /// The shard gate for a reboot of `ip` of `kind`: the kind to run, or
    /// `None` for a clean restart in place instead (see the
    /// [`shard`](super::shard) module).
    fn gate_reboot(&self, ip: std::net::IpAddr, kind: RebootKind) -> Option<RebootKind> {
        let kind = self.gate_crash(ip, kind);
        if kind.is_none() {
            assert_reachable!("shard gate: a kill becomes a clean restart");
            if let Some(book) = ShardBook::of(&self.state) {
                book.count(Downgrade::KillToRestart);
            }
        }
        kind
    }

    /// The gate's judgement of `kind` on `ip`: a wipe that would cost a
    /// shard its data becomes a crash, a crash whose dirty sectors could
    /// cost a stripe keeps the page cache, and `None` when taking `ip` down
    /// would stop a shard's progress. Draws nothing.
    fn gate_crash(&self, ip: std::net::IpAddr, kind: RebootKind) -> Option<RebootKind> {
        let Some((book, requirements)) = self.gate_requirements() else {
            return Some(kind);
        };
        let verdict = super::shard::pure(|| {
            let mut kind = kind;
            if kind == RebootKind::CrashAndWipe {
                let not_intact = self.not_intact(&book, &requirements);
                if !super::shard::lose_ok(&requirements, &not_intact, &[ip]) {
                    assert_reachable!("shard gate: a wipe becomes a crash");
                    book.count(Downgrade::WipeToCrash);
                    kind = RebootKind::Crash;
                }
            }
            let dead = self.dead_set();
            let cut = self.cut();
            if book.config().judges_progress()
                && !super::shard::down_ok(&requirements, &dead, &[ip], &cut)
            {
                return None;
            }
            if kind == RebootKind::Crash && self.sim.crash_endangers_shards(ip) {
                assert_reachable!("shard gate: a crash keeps the page cache");
                book.count(Downgrade::CrashToKeepCache);
                kind = RebootKind::KillKeepCache;
            }
            Some(kind)
        });
        if verdict.is_some() {
            book.note_fault(ip);
        }
        verdict
    }

    /// The run's shards, read-only: the book of every declaration, its
    /// counts, and the disks' ground truth (see [`Shards`]).
    #[must_use]
    pub fn shards(&self) -> Shards {
        Shards::new(
            ShardBook::of(&self.state).unwrap_or_default(),
            self.sim.downgrade(),
        )
    }

    /// Explicitly restart a process, typically one held down by
    /// [`crash`](Self::crash).
    ///
    /// Schedules a `ProcessRestart` event: the orchestrator boots a fresh
    /// instance from the process factory and the process leaves
    /// [`dead_count`](Self::dead_count).
    ///
    /// Restarting a process that is still running is an in-place restart:
    /// the old boot is killed like a force kill — its task, the tasks it
    /// spawned and its connections (peers see them aborted); storage is
    /// left alone — and recorded as a `ProcessForceKill` fault with cause
    /// [`ProcessKillKind::RestartInPlace`].
    /// The fresh instance boots one scheduler tick later, once the executor
    /// has dropped the old boot, so two boots of one process never run side
    /// by side.
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails.
    pub fn restart(&self, ip: &str) -> SimulationResult<()> {
        let ip_addr = parse_ip(ip)?;
        if self.retired(ip_addr) {
            return Ok(());
        }
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
    /// Picks a random process among those not currently dead and reboots it.
    /// Returns `Ok(None)` if every process is dead (or there are none).
    ///
    /// # Errors
    ///
    /// Returns an error if IP parsing fails or the operation is rejected by the simulator.
    pub fn reboot_random(&self, kind: RebootKind) -> SimulationResult<Option<String>> {
        let alive: Vec<String> = self
            .process_ips()
            .into_iter()
            .filter(|ip| !self.is_dead(ip))
            .collect();
        if alive.is_empty() {
            return Ok(None);
        }
        let idx = crate::sim::sim_random_range(0..alive.len());
        let ip = alive[idx].clone();
        self.reboot(&ip, kind)?;
        Ok(Some(ip))
    }

    /// Reboot all processes matching a tag key=value pair. Processes already
    /// dead are skipped (see [`reboot`](Self::reboot)).
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
            .membership
            .ips_tagged(key, value)
            .into_iter()
            .map(|ip| ip.to_string())
            .collect();

        for ip in &matching_ips {
            self.reboot(ip, kind)?;
        }

        Ok(matching_ips)
    }

    /// A copy of the live machine registry, for failure-domain queries
    /// during fault injection.
    #[must_use]
    pub fn machine_registry(&self) -> MachineRegistry {
        self.process_info.membership.machine_registry()
    }

    /// Whether `ip` was retired: faults leave it alone.
    fn retired(&self, ip: std::net::IpAddr) -> bool {
        let retired = self.process_info.membership.is_retired(ip);
        if retired {
            assert_reachable!("deployment: a fault aimed at a retired process is dropped");
        }
        retired
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
    /// Processes already dead are skipped (see [`reboot`](Self::reboot)).
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
            .membership
            .machine_registry()
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

/// The force-kill cause for a crash of `kind`.
fn kill_cause(kind: RebootKind) -> ProcessKillKind {
    match kind {
        RebootKind::KillKeepCache => ProcessKillKind::KillKeepCache,
        RebootKind::CrashAndWipe => ProcessKillKind::CrashAndWipe,
        RebootKind::Graceful | RebootKind::Crash => ProcessKillKind::Crash,
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

    /// The attrition regime this injector runs, if it is the built-in
    /// attrition injector: the regime that decides a process's
    /// [reboot hints](moonpool_buggify::hint) too. Custom injectors keep
    /// the default `None`.
    #[doc(hidden)]
    fn hint_regime(&self) -> Option<super::process::Attrition> {
        None
    }
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

    /// `kind`, or [`RebootKind::Crash`] when it is a wipe of `victims`
    /// that would pass the regime's [`WipeBudget`](super::wipe::WipeBudget)
    /// over the victim pool `eligible`: the pool's unhealed processes other
    /// than the victims (re-wiping an unhealed process loses nothing more),
    /// plus the victims, must stay within `max_unhealed`. Called after the
    /// kind is drawn; draws nothing, and without a budget returns `kind`
    /// unread.
    fn within_wipe_budget(
        &self,
        ctx: &FaultContext,
        kind: RebootKind,
        eligible: &[&String],
        victims: &[&String],
    ) -> RebootKind {
        let Some(budget) = self.config.wipe_budget else {
            return kind;
        };
        if kind != RebootKind::CrashAndWipe {
            return kind;
        }
        let others: Vec<&String> = eligible
            .iter()
            .copied()
            .filter(|ip| !victims.contains(ip))
            .collect();
        let unhealed = WipeLedger::of(ctx.state()).map_or(0, |ledger| {
            ledger.unhealed_among(&others, budget.heal, ctx.sim.current_time())
        });
        if unhealed + victims.len() > budget.max_unhealed {
            assert_reachable!("attrition: wipe budget downgrades a wipe to a crash");
            tracing::info!(
                unhealed,
                victims = victims.len(),
                max_unhealed = budget.max_unhealed,
                "attrition: wipe budget spent, crashing instead of wiping"
            );
            return RebootKind::Crash;
        }
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
    pub(crate) fn admits(&self, ctx: &FaultContext, ip: &str) -> bool {
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
        if ip.parse().is_ok_and(|ip| ctx.membership().was_spawned(ip)) {
            assert_reachable!("deployment: attrition picks a process spawned mid-run");
        }
    }

    /// Reboot `ip` at its own [hint](moonpool_buggify::hint), under this
    /// regime: within the `max_dead` budget of the eligible pool, with a
    /// kind and delays drawn as for a timed reboot. Returns whether the
    /// reboot was scheduled. The hinting process alone is rebooted, whatever
    /// the regime's failure scope. `permit` is asked last, once the reboot
    /// fits the budget, and draws nothing: a refusal leaves the process
    /// alive.
    pub(crate) fn reboot_on_hint(
        &self,
        ctx: &FaultContext,
        ip: &str,
        permit: impl FnOnce() -> bool,
    ) -> SimulationResult<bool> {
        if ctx.is_dead(ip) {
            return Ok(false);
        }
        let ips = ctx.process_ips();
        let eligible = self.eligible(ctx, &ips);
        if Self::dead_among(ctx, &eligible) >= self.config.max_dead {
            assert_reachable!("attrition: max_dead limit refuses a reboot hint");
            return Ok(false);
        }
        if !permit() {
            return Ok(false);
        }
        let ip_string = ip.to_string();
        let kind = self.within_wipe_budget(ctx, self.choose_kind(), &eligible, &[&ip_string]);
        let (recovery_range, grace_range) = self.delay_ranges();
        self.check_victim(ctx, ip);
        ctx.reboot_with_delays(ip, kind, &recovery_range, &grace_range)?;
        Ok(true)
    }

    /// Reboot a single random live eligible process, respecting the
    /// `max_dead` budget over the whole eligible pool.
    fn inject_process(&self, ctx: &FaultContext) -> SimulationResult<()> {
        let ips = ctx.process_ips();
        let eligible = self.eligible(ctx, &ips);
        if Self::dead_among(ctx, &eligible) >= self.config.max_dead {
            assert_reachable!("attrition: max_dead limit enforced");
            return Ok(());
        }
        // Only a live process can be killed: drawing a dead one would schedule
        // a second restart for it.
        let live: Vec<&String> = eligible
            .iter()
            .copied()
            .filter(|ip| !ctx.is_dead(ip))
            .collect();
        if live.is_empty() {
            assert_reachable!("attrition: no eligible victim");
            return Ok(());
        }
        if self.edge_round(ctx, &live)? {
            return Ok(());
        }
        let drawn = self.choose_kind();
        let (recovery_range, grace_range) = self.delay_ranges();
        let idx = Self::draw_victim(ctx, &live);
        let ip = live[idx].clone();
        // Judged once the victim is drawn, so the draw order is unchanged.
        let kind = self.within_wipe_budget(ctx, drawn, &eligible, &[&ip]);
        assert_sometimes_each!(
            "attrition_process_targeted",
            [("process_idx", i64::try_from(idx).unwrap_or(i64::MAX))]
        );
        // The harness's budget (`HintVeto`) covers a timed reboot as it
        // covers a hint's: it lands at any instant, inside a commit too.
        if !super::hint::veto_permits(ctx, &ip, super::hint::TIMED_REBOOT_LABEL) {
            assert_reachable!("attrition: the harness's veto refuses a timed reboot");
            return Ok(());
        }
        self.check_victim(ctx, &ip);
        ctx.reboot_with_delays(&ip, kind, &recovery_range, &grace_range)
    }

    /// The index of the victim among `live`: uniform, or, under an active
    /// [aim](super::shard::ShardFaults::aim), weighted toward the replicas
    /// of moving shards. One draw either way.
    fn draw_victim(ctx: &FaultContext, live: &[&String]) -> usize {
        let aimed = Self::aim(ctx).filter(|(_, aimed)| {
            live.iter()
                .any(|ip| ip.parse().is_ok_and(|ip| aimed.contains(&ip)))
        });
        let Some((factor, aimed)) = aimed else {
            return crate::sim::sim_random_range(0..live.len());
        };
        let weights: Vec<f64> = live
            .iter()
            .map(|ip| {
                if ip.parse().is_ok_and(|ip| aimed.contains(&ip)) {
                    factor
                } else {
                    1.0
                }
            })
            .collect();
        let total: f64 = weights.iter().sum();
        let mut point = crate::sim::sim_random_f64() * total;
        let mut idx = live.len() - 1;
        for (at, weight) in weights.iter().enumerate() {
            if point < *weight {
                idx = at;
                break;
            }
            point -= weight;
        }
        if weights[idx] > 1.0
            && let Some(book) = ShardBook::of(ctx.state())
        {
            assert_reachable!("shard aim: attrition strikes a moving shard");
            book.count_aimed();
        }
        idx
    }

    /// The active aim: its factor and the processes it points at, when an
    /// aim is configured and an active site's move is in flight.
    fn aim(ctx: &FaultContext) -> Option<(f64, std::collections::BTreeSet<std::net::IpAddr>)> {
        let book = ShardBook::of(ctx.state())?;
        let aim = book.config().aim_config()?;
        let aimed = book.aimed();
        (!aimed.is_empty()).then_some((aim.factor(), aimed))
    }

    /// An [edge](super::shard::ShardFaults::edge) round, when the edge
    /// draw says so: pick a shard with a live eligible replica outside one
    /// of its minimal sets, and take down every live eligible replica
    /// outside that set. Returns whether the round was an edge round.
    fn edge_round(&self, ctx: &FaultContext, live: &[&String]) -> SimulationResult<bool> {
        let Some(book) = ShardBook::of(ctx.state()) else {
            return Ok(false);
        };
        let edge = book.config().edge_fraction();
        if edge <= 0.0 || !crate::sim::sim_random_bool(edge) {
            return Ok(false);
        }
        let live_ips: Vec<std::net::IpAddr> =
            live.iter().filter_map(|ip| ip.parse().ok()).collect();
        let dead = ctx.dead_set();
        let mut candidates: Vec<(Vec<std::net::IpAddr>, Vec<Vec<std::net::IpAddr>>)> = Vec::new();
        for (_, replications) in book.requirements() {
            let need = moonpool_buggify::shard::Need::All(
                replications
                    .iter()
                    .map(|replication| replication.progress.clone())
                    .collect(),
            );
            let up = |replica: &moonpool_buggify::shard::Replica| {
                super::shard::replica_ip(replica).is_none_or(|ip| !dead.contains(&ip))
            };
            let sets: Vec<Vec<std::net::IpAddr>> = need
                .minimal_sets(&up)
                .iter()
                .map(|set| set.iter().filter_map(super::shard::replica_ip).collect())
                .collect();
            let replicas: Vec<std::net::IpAddr> = replications
                .iter()
                .flat_map(|replication| {
                    replication
                        .replicas
                        .iter()
                        .chain(&replication.progress.replicas())
                        .filter_map(super::shard::replica_ip)
                        .collect::<Vec<_>>()
                })
                .filter(|ip| live_ips.contains(ip))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            if sets
                .iter()
                .any(|set| replicas.iter().any(|ip| !set.contains(ip)))
            {
                candidates.push((replicas, sets));
            }
        }
        if candidates.is_empty() {
            assert_reachable!("shard edge: no shard is above its minimum");
            return Ok(false);
        }
        let (replicas, sets) = &candidates[crate::sim::sim_random_range(0..candidates.len())];
        let set = &sets[crate::sim::sim_random_range(0..sets.len())];
        let kind = self.choose_kind();
        let (recovery_range, grace_range) = self.delay_ranges();
        for ip in replicas.iter().filter(|ip| !set.contains(ip)) {
            let ip = ip.to_string();
            self.check_victim(ctx, &ip);
            ctx.reboot_with_delays(&ip, kind, &recovery_range, &grace_range)?;
        }
        assert_reachable!("shard edge: a shard is pushed to its minimum");
        book.count_edge();
        Ok(true)
    }

    /// Reboot every live eligible process in a random failure domain
    /// *together*, only if the group fits within the `max_dead` budget over
    /// the eligible pool. A no-op when no locality topology is configured
    /// (`domains` empty). Domains holding no live eligible process are never
    /// drawn, so neither the victim filter nor earlier kills can make a round
    /// silently pick an empty group.
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
            self.eligible(ctx, &all)
                .into_iter()
                .filter(|ip| !ctx.is_dead(ip))
                .cloned()
                .collect()
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
        let pool = ctx.process_ips();
        let eligible = self.eligible(ctx, &pool);
        if Self::dead_among(ctx, &eligible) + ips.len() > self.config.max_dead {
            assert_reachable!("attrition: max_dead limit enforced (group)");
            return Ok(());
        }
        let victims: Vec<&String> = ips.iter().collect();
        let kind = self.within_wipe_budget(ctx, self.choose_kind(), &eligible, &victims);
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

    fn hint_regime(&self) -> Option<super::process::Attrition> {
        Some(self.config.clone())
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        while !ctx.chaos_shutdown().is_cancelled() {
            // Random delay between reboot attempts (1-5 seconds), shorter
            // by the aim's factor while it points at a move.
            let delay = Duration::from_millis(
                u64::try_from(crate::sim::sim_random_range(1000..5000))
                    .expect("delay_ms is non-negative"),
            );
            let delay = match Self::aim(ctx) {
                Some((factor, _)) if self.config.scope == AttritionScope::PerProcess => {
                    delay.div_f64(factor).max(Duration::from_millis(1))
                }
                _ => delay,
            };
            ctx.time()
                .sleep(delay)
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
