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

    /// Instant kill: the task is cancelled and all connections abort at the
    /// crash instant, not after the recovery delay.
    ///
    /// No buffer drain. Peers see connection reset errors. Unsynced storage
    /// data may be lost. The process performs no further work until it is
    /// restarted.
    Crash,

    /// Instant kill + wipe all storage for this process.
    ///
    /// Same as [`Crash`](RebootKind::Crash) but also deletes all persistent
    /// storage owned by this process's IP. Simulates total data loss or a
    /// new node joining the cluster.
    CrashAndWipe,
}

/// The failure domain a reboot targets.
///
/// Controls how [`AttritionInjector`](super::fault_injector) selects victims.
/// `PerMachine` / `PerZone` / `PerDatacenter` reboot all collocated processes
/// *together* (modeling correlated failure) and require a
/// [`.cluster()`](super::builder::SimulationBuilder::cluster) topology. Without
/// locality they are a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AttritionScope {
    /// Reboot a single random process (the historical behavior).
    #[default]
    PerProcess,
    /// Reboot every process on a single random machine atomically.
    PerMachine,
    /// Reboot every process in a single random zone atomically.
    PerZone,
    /// Reboot every process in a single random datacenter atomically.
    ///
    /// The widest correlated failure: a whole region going down. Only fires when
    /// the entire datacenter fits in the remaining `max_dead` budget, so a
    /// datacenter-sized budget is needed for it to ever trigger.
    PerDatacenter,
}

/// Which server processes the built-in attrition may reboot.
///
/// A system under test with more than one **role** — a consensus tier plus a
/// pool of spares, acceptors plus matchmakers — rarely wants every process to
/// be an equally likely victim: a campaign that draws uniformly over the whole
/// pool spends most of its kills on idle spares. The filter narrows the draw to
/// one process group (a `.processes()` / `.cluster()` registration, named after
/// its process type) or to processes carrying one tag.
///
/// The filter never consumes randomness of its own. The victim is drawn
/// uniformly over the *eligible* processes, so a filter that excludes nothing
/// leaves every seed's draw schedule exactly as it was, and a filter that
/// leaves nothing eligible makes the injector skip the round without drawing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AttritionVictims {
    /// Every server process is eligible (the default).
    #[default]
    Any,
    /// Only processes registered in the named group — see
    /// [`SimulationBuilder::processes`](super::builder::SimulationBuilder::processes).
    Group(String),
    /// Only processes tagged `key=value` — see
    /// [`SimulationBuilder::tags`](super::builder::SimulationBuilder::tags).
    Tagged {
        /// The tag key.
        key: String,
        /// The tag value.
        value: String,
    },
}

impl AttritionVictims {
    /// Restrict attrition to the process group named `group`.
    #[must_use]
    pub fn group(group: impl Into<String>) -> Self {
        Self::Group(group.into())
    }

    /// Restrict attrition to processes tagged `key=value`.
    #[must_use]
    pub fn tagged(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self::Tagged {
            key: key.into(),
            value: value.into(),
        }
    }

    /// Whether `ip` may be rebooted under this filter.
    #[must_use]
    pub fn admits(&self, ip: std::net::IpAddr, info: &super::fault_injector::ProcessInfo) -> bool {
        match self {
            Self::Any => true,
            Self::Group(group) => info.group_registry.group_for(ip) == Some(group.as_str()),
            Self::Tagged { key, value } => info
                .tag_registry
                .tags_for(ip)
                .is_some_and(|tags| tags.matches(key, value)),
        }
    }
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
///     scope: AttritionScope::PerProcess,
///     victims: AttritionVictims::Any,
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Attrition {
    /// Maximum number of simultaneously dead processes **among this
    /// injector's victims**.
    ///
    /// The attrition injector will not kill a process if the number of currently
    /// dead (not yet restarted) processes in its victim pool is already at this
    /// limit. With [`AttritionVictims::Any`] the pool is the whole cluster; with
    /// a group or tag filter it is that pool alone, so two injectors with
    /// different pools budget independently ("at most one dead acceptor *and*
    /// at most two dead matchmakers").
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

    /// The failure domain each reboot targets.
    ///
    /// [`AttritionScope::PerProcess`] (the default) kills one random process at
    /// a time. [`AttritionScope::PerMachine`], [`AttritionScope::PerZone`], and
    /// [`AttritionScope::PerDatacenter`] kill all collocated processes together,
    /// modeling correlated failure; they require a
    /// [`.cluster()`](super::builder::SimulationBuilder::cluster) topology and
    /// respect `max_dead` as a whole-group budget.
    pub scope: AttritionScope,

    /// Which processes may be chosen as victims.
    ///
    /// [`AttritionVictims::Any`] (the default) draws over every server
    /// process. A group or tag filter keeps attrition on the role under test
    /// — "acceptors, never spares" — scopes `max_dead` to that pool, and
    /// applies to every scope: a machine-, zone-, or datacenter-scoped reboot
    /// only picks domains holding an eligible process and only reboots the
    /// eligible processes in it. Register one [`Chaos::Attrition`](super::config::Chaos::Attrition)
    /// entry per pool to give each process group its own regime.
    pub victims: AttritionVictims,
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
    /// Draws exactly six values from the independent
    /// `CONFIG_RNG` stream (fixed sequence ⇒ reproducible per seed, never
    /// perturbs in-run randomness). The first draw, with ~50% probability, sets
    /// `max_dead = 0` — the never-reboot regime, where the injector's
    /// `dead_count() >= max_dead` gate is always true so it never reboots. The
    /// remaining three each mask one reboot-kind weight to `0.0` with ~50%
    /// probability. A fifth draw scales the configured recovery-delay range to
    /// 50%-200% of its pinned values. The sixth is reserved for failure scope;
    /// the runner uses it to select a topology-backed scope whose groups can fit
    /// within `max_dead`.
    ///
    /// When all three kind weights are masked off, [`choose_kind`](Self::choose_kind)
    /// falls back to [`RebootKind::Crash`] — the "always crash" single-mode regime.
    #[must_use]
    pub fn swarm_for_seed(&self) -> Attrition {
        self.swarm_for_seed_with_topology(None)
    }

    /// Derive a swarm regime with failure scopes constrained by `topology`.
    pub(crate) fn swarm_for_seed_with_topology(
        &self,
        topology: Option<&super::locality::MachineRegistry>,
    ) -> Attrition {
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

        // Scale the whole window rather than sampling the eventual delay here:
        // the injector still performs its usual single SIM_RNG draw, preserving
        // counted-stream call positions and exploration recipes.
        let recovery = self.recovery_delay_ms.clone().unwrap_or(1000..10000);
        let scale_percent = crate::sim::rng::config_random_range(50..201);
        regime.recovery_delay_ms = Some(Self::scaled_range(recovery, scale_percent));

        // Twelve is divisible by every possible candidate count (1..=4), so
        // reducing it modulo the viable set does not bias one scope.
        let scope_draw = crate::sim::rng::config_random_range(0..12);
        if let Some(topology) = topology.filter(|topology| !topology.is_empty()) {
            let scopes = Self::viable_scopes(topology, regime.max_dead);
            regime.scope = scopes[scope_draw % scopes.len()];
        }
        regime
    }

    fn scaled_range(range: Range<usize>, percent: usize) -> Range<usize> {
        let mut start = range.start.saturating_mul(percent) / 100;
        let mut end = range.end.saturating_mul(percent) / 100;
        if end <= start {
            end = start.saturating_add(1);
            if end == start {
                start = start.saturating_sub(1);
            }
        }
        start..end
    }

    fn viable_scopes(
        topology: &super::locality::MachineRegistry,
        max_dead: usize,
    ) -> Vec<AttritionScope> {
        let mut scopes = vec![AttritionScope::PerProcess];
        let candidates = [
            (
                AttritionScope::PerMachine,
                crate::locality::DomainLevel::Machine,
                topology.all_machines(),
            ),
            (
                AttritionScope::PerZone,
                crate::locality::DomainLevel::Zone,
                topology.all_zones(),
            ),
            (
                AttritionScope::PerDatacenter,
                crate::locality::DomainLevel::Datacenter,
                topology.all_datacenters(),
            ),
        ];
        for (scope, level, domains) in candidates {
            let fits_budget = domains.iter().all(|domain| {
                let group_size = topology.ips_in_domain(level, domain).len();
                group_size > 0 && group_size <= max_dead
            });
            if fits_budget {
                scopes.push(scope);
            }
        }
        scopes
    }
}

#[cfg(test)]
mod swarm_tests {
    use std::net::IpAddr;

    use super::{Attrition, AttritionScope, AttritionVictims};
    use crate::sim::rng::{rng_call_count, set_config_seed, set_sim_seed};
    use crate::{LocalityInfo, runner::locality::MachineRegistry};

    /// A representative base regime: all three reboot kinds enabled.
    fn base() -> Attrition {
        Attrition {
            max_dead: 2,
            prob_graceful: 0.3,
            prob_crash: 0.5,
            prob_wipe: 0.2,
            recovery_delay_ms: None,
            grace_period_ms: None,
            scope: AttritionScope::PerProcess,
            victims: AttritionVictims::Any,
        }
    }

    #[test]
    fn swarm_keeps_the_victim_filter() {
        set_config_seed(7);
        let mut filtered = base();
        filtered.victims = AttritionVictims::group("acceptor");
        assert_eq!(
            filtered.swarm_for_seed().victims,
            AttritionVictims::group("acceptor")
        );
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

    #[test]
    fn swarm_varies_recovery_window_across_seeds() {
        let windows = (0..100_u64)
            .map(|seed| swarm_for(seed).recovery_delay_ms)
            .collect::<Vec<_>>();
        let first = windows[0].clone();
        assert!(
            windows.iter().any(|window| *window != first),
            "swarm did not vary the recovery window across 100 seeds"
        );
    }

    #[test]
    fn attrition_swarm_never_advances_the_counted_rng() {
        set_sim_seed(42);
        set_config_seed(42);
        let before = rng_call_count();

        let _ = base().swarm_for_seed();

        assert_eq!(rng_call_count(), before);
    }

    fn topology(processes_per_machine: usize) -> MachineRegistry {
        let mut topology = MachineRegistry::new();
        let mut process = 1_u8;
        for machine in 1..=2 {
            for _ in 0..processes_per_machine {
                topology.register(
                    IpAddr::from([10, 0, 1, process]),
                    LocalityInfo::new(
                        format!("dc{machine}"),
                        format!("dc{machine}-z1"),
                        format!("dc{machine}-z1-m1"),
                    ),
                );
                process += 1;
            }
        }
        topology
    }

    #[test]
    fn clustered_swarm_varies_viable_attrition_scope() {
        let topology = topology(1);
        let scopes = (0..100_u64)
            .map(|seed| {
                set_config_seed(seed);
                base().swarm_for_seed_with_topology(Some(&topology)).scope
            })
            .collect::<Vec<_>>();

        assert!(
            scopes
                .iter()
                .any(|scope| *scope != AttritionScope::PerProcess),
            "clustered swarm did not select a correlated failure scope"
        );
    }

    #[test]
    fn clustered_swarm_rejects_scopes_that_exceed_max_dead() {
        let topology = topology(3);
        for seed in 0..100_u64 {
            set_config_seed(seed);
            let regime = base().swarm_for_seed_with_topology(Some(&topology));
            assert_eq!(regime.scope, AttritionScope::PerProcess);
        }
    }
}
