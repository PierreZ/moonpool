//! Public configuration types used by [`super::SimulationBuilder`].

use std::ops::{Range, RangeInclusive};

use super::process::Attrition;

/// Configuration for how many iterations a simulation should run.
#[derive(Debug, Clone)]
pub enum IterationControl {
    /// Run a fixed number of iterations with specific seeds.
    FixedCount(usize),
    /// Stop when every observed sometimes/reachable assertion has fired and
    /// code coverage has not grown for several consecutive seeds.
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
                let start = *range.start();
                let end = *range.end() + 1;
                if start >= end {
                    start
                } else {
                    crate::sim::sim_random_range(start..end)
                }
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
/// enabled and of workload operation-alphabet swarming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChaosMode {
    /// Enable the complete surface and randomize its intensities each seed.
    Random,
    /// Enable a random subset of the surface's fault families each seed.
    Swarm,
}

/// A chaos surface to enable and its per-seed sampling strategy.
#[derive(Debug, Clone, PartialEq)]
pub enum Chaos {
    /// Network faults such as partitions, corruption, and random close.
    Network(ChaosMode),
    /// Storage faults such as corruption, misdirection, and sync failure.
    Storage(ChaosMode),
    /// Process attrition using the supplied reboot regime.
    Attrition {
        /// Base reboot regime.
        config: Attrition,
        /// Per-seed sampling strategy.
        mode: ChaosMode,
    },
    /// Occasionally perturb enabled network and storage knobs to extremes.
    BuggifyKnobs,
}
