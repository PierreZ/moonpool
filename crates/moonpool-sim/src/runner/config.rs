//! Public configuration types used by [`super::SimulationBuilder`].

use std::ops::{Range, RangeInclusive};

use super::process::Attrition;

/// When a campaign stops running new seeds.
///
/// Set it with [`SimulationBuilder::set_iterations`](super::SimulationBuilder::set_iterations)
/// or [`SimulationBuilder::until_coverage_stable`](super::SimulationBuilder::until_coverage_stable).
/// A deadlocked seed stops the campaign under either rule.
#[derive(Debug, Clone)]
pub enum IterationControl {
    /// Run exactly this many seeds: the pinned ones from
    /// [`set_debug_seeds`](super::SimulationBuilder::set_debug_seeds) first,
    /// then runner-drawn ones.
    FixedCount(usize),
    /// Stop when every observed `assert_sometimes!` / `assert_reachable!`
    /// fired and code coverage has not grown for `plateau_seeds`
    /// consecutive seeds, or after `max_iterations` seeds. This is the
    /// default (`plateau_seeds: 10`, `max_iterations: 1000`).
    ///
    /// Coverage is real LLVM sancov edge coverage when the binary is
    /// instrumented (`cargo xtask sim run`), otherwise assertion-slot
    /// coverage.
    UntilCoverageStable {
        /// Number of consecutive seeds without coverage growth required to stop.
        plateau_seeds: usize,
        /// Maximum number of seeds before stopping regardless.
        max_iterations: usize,
    },
}

/// How many instances of a workload to spawn per iteration.
#[derive(Debug, Clone)]
pub enum WorkloadCount {
    /// Spawn exactly this many instances every iteration.
    Fixed(usize),
    /// Draw the instance count from this half-open range for each seed.
    Random(Range<usize>),
}

impl WorkloadCount {
    pub(super) fn resolve(&self) -> usize {
        match self {
            Self::Fixed(count) => *count,
            Self::Random(range) => crate::sim::sim_random_range(range.clone()),
        }
    }
}

/// How many process instances to spawn per iteration.
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessCount {
    /// Spawn exactly this many process instances every iteration.
    Fixed(usize),
    /// Draw the process count from this inclusive range for each seed.
    Range(RangeInclusive<usize>),
}

impl ProcessCount {
    pub(crate) fn resolve(&self) -> usize {
        match self {
            Self::Fixed(count) => *count,
            Self::Range(range) => {
                crate::sim::sim_random_range_or_default(*range.start()..*range.end() + 1)
            }
        }
    }
}

impl From<usize> for ProcessCount {
    fn from(count: usize) -> Self {
        Self::Fixed(count)
    }
}

impl From<RangeInclusive<usize>> for ProcessCount {
    fn from(range: RangeInclusive<usize>) -> Self {
        Self::Range(range)
    }
}

/// How an enabled chaos surface is sampled each seed.
///
/// This sampling strategy is independent of which [`Chaos`] surface is
/// enabled and of workload operation-alphabet swarming
/// ([`SimulationBuilder::swarm_operations`](super::SimulationBuilder::swarm_operations)).
///
/// Prefer `Swarm`. When every family is a little bit on in every seed, the
/// families crowd each other out, and the extreme configurations that find
/// bugs (only partitions, at a high rate) almost never happen. Swarm
/// testing turns most families fully off on each seed, so each seed is a
/// different, sharper experiment. All decisions are draws on the
/// simulation stream, so a seed always rebuilds the same subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChaosMode {
    /// Enable the complete surface and randomize its intensities each seed
    /// (`random_for_seed` on the surface's configuration).
    Random,
    /// Enable a random subset of the surface's fault families each seed and
    /// turn the rest fully off (`swarm_for_seed` on the surface's
    /// configuration).
    Swarm,
}

impl ChaosMode {
    /// Resolve one chaos surface's per-seed configuration from its mode: the
    /// single dispatch every surface (network, storage, attrition) goes
    /// through.
    ///
    /// `mode` is `None` for a surface that was not enabled, which takes `off`.
    /// Exactly one of the three constructors runs, and only `swarm` and
    /// `random` may draw from the simulation stream.
    ///
    /// # Draw-order contract
    ///
    /// The *call sites* of this function are load-bearing for replay and for
    /// exploration recipes, because the constructors draw from `SIM_RNG` and
    /// every draw's position is part of a seed's schedule:
    ///
    /// 1. network, then storage, in `build_sim_for_iteration`, **before**
    ///    `SimWorld::new_with_network_config_and_seed` resets and reseeds the
    ///    stream — so these draws are not counted in the run's
    ///    `rng_call_count` and an exploration breakpoint can never land on
    ///    them;
    /// 2. then the operation-alphabet swarm mask and `set_rng_breakpoints`;
    /// 3. then attrition, in registration order, in
    ///    `run_orchestrator_for_iteration` — **after** the reset and the
    ///    breakpoints, so its draws are counted and replayed;
    /// 4. then outages, in registration order, the same way;
    /// 5. then (after the replicated storage fault patterns) the CPU model,
    ///    then the gray-failure regimes in registration order. Both draw
    ///    only when registered, so opting in shifts no other surface.
    ///
    /// Moving a call across the reset, or reordering two calls, shifts every
    /// seed and invalidates recorded recipes.
    pub(crate) fn resolve<T>(
        mode: Option<Self>,
        off: impl FnOnce() -> T,
        random: impl FnOnce() -> T,
        swarm: impl FnOnce() -> T,
    ) -> T {
        match mode {
            None => off(),
            Some(Self::Random) => random(),
            Some(Self::Swarm) => swarm(),
        }
    }
}

/// A chaos surface to enable and its per-seed sampling strategy.
///
/// Pass a list of them to
/// [`SimulationBuilder::enable_chaos`](super::SimulationBuilder::enable_chaos).
/// A surface not listed stays off. Every surface that injects faults over
/// time (attrition, outages, gray failures) runs only inside the chaos
/// window, so it needs
/// [`SimulationBuilder::chaos_duration`](super::SimulationBuilder::chaos_duration).
/// When the window closes,
/// [`SimWorld::enter_recovery_mode`](crate::SimWorld::enter_recovery_mode)
/// turns the configuration-driven network and storage faults off.
#[derive(Debug, Clone, PartialEq)]
pub enum Chaos {
    /// Network faults: clogs, partitions, bit flips, random closes, connect
    /// failures, clock drift, buggified delays, slow IP pairs and black
    /// holes (see [`NetworkFault`](crate::NetworkFault) and
    /// [`ChaosConfiguration`](crate::ChaosConfiguration)).
    ///
    /// `Random` samples [`NetworkConfiguration::random_for_seed`](crate::NetworkConfiguration::random_for_seed);
    /// `Swarm` samples [`NetworkConfiguration::swarm_for_seed`](crate::NetworkConfiguration::swarm_for_seed).
    /// [`SimulationBuilder::network_fault_mask`](super::SimulationBuilder::network_fault_mask)
    /// removes families afterward. A later entry replaces an earlier one.
    ///
    /// Without this entry the network is not fault-free: it uses
    /// [`NetworkConfiguration::default`](crate::NetworkConfiguration::default),
    /// with `FoundationDB`'s always-on rates (rare random closes, clock
    /// drift, buggified delays, buggified connects that fail or hang).
    Network(ChaosMode),
    /// Storage faults: corruption, `EIO`, misdirected I/O, phantom writes,
    /// sync failures, short transfers, directory-entry loss, crash damage,
    /// disk stalls and throttles, and disk failure (see
    /// [`StorageFault`](crate::StorageFault) and
    /// [`StorageConfiguration`](crate::StorageConfiguration)).
    ///
    /// `Random` samples [`StorageConfiguration::random_for_seed`](crate::StorageConfiguration::random_for_seed);
    /// `Swarm` samples [`StorageConfiguration::swarm_for_seed`](crate::StorageConfiguration::swarm_for_seed).
    /// [`SimulationBuilder::storage_fault_mask`](super::SimulationBuilder::storage_fault_mask)
    /// removes families afterward. A later entry replaces an earlier one.
    Storage(ChaosMode),
    /// Process reboots under a [`Attrition`] regime: graceful shutdowns,
    /// crashes and crash-and-wipes, never more than `max_dead` of the
    /// regime's victims down at once.
    ///
    /// Each entry adds one independent injector, so each process group can
    /// have its own regime ([`AttritionVictims`](crate::AttritionVictims)).
    /// The regime also decides whether a [`hint!`](crate::hint!) point may
    /// reboot its process.
    Attrition {
        /// Base reboot regime.
        config: Attrition,
        /// Per-seed sampling strategy: `Random` uses the regime as written;
        /// `Swarm` draws a per-seed variant of it
        /// ([`Attrition::swarm_for_seed`]), including a regime that never
        /// reboots.
        mode: ChaosMode,
    },
    /// A correlated outage: every process of the named groups loses power
    /// at one instant, each back after its own delay (see
    /// [`Outage`](crate::Outage)). At most one per run, inside the chaos
    /// window; its restarts run even once the window has closed.
    Outage {
        /// The outage regime.
        config: crate::Outage,
        /// Per-seed sampling strategy: `Random` keeps the outage on every
        /// seed (it still strikes with its `probability`); `Swarm` drops it
        /// on half the seeds, and its straggler on half the rest.
        mode: ChaosMode,
    },
    /// On some seeds, push the knobs of the enabled network and storage
    /// surfaces to extremes (`apply_buggify_knobs` on
    /// [`ChaosConfiguration`](crate::ChaosConfiguration) and
    /// [`StorageConfiguration`](crate::StorageConfiguration)), the way
    /// `FoundationDB` randomizes its knobs under `BUGGIFY`. It has no effect
    /// on a surface that is off.
    BuggifyKnobs,
    /// The CPU model: every poll of a process's task takes CPU time on the
    /// process's one core (see [`CpuModel`](crate::CpuModel) and
    /// [`SimWorld::set_cpu_model`](crate::SimWorld::set_cpu_model)).
    /// `Random` turns it on for every seed with per-seed costs; `Swarm` for
    /// half of the seeds. A seed with the model off runs exactly as without
    /// this entry, apart from the draws that decided it.
    Cpu(ChaosMode),
    /// Gray failures: one or more machines stay alive and slow for a long
    /// window (see [`GrayFailure`](crate::GrayFailure)). At most one regime
    /// is useful per victim pool; each entry adds one injector.
    GrayFailure {
        /// The gray-failure regime.
        config: crate::GrayFailure,
        /// Per-seed sampling strategy: `Random` applies every configured
        /// kind; `Swarm` masks each kind (CPU, disk, network) per seed.
        mode: ChaosMode,
    },
}
