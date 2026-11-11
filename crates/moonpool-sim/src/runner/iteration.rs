//! Iteration control and deterministic seed generation.

use super::builder::IterationControl;
use super::wall_clock;

/// The `SplitMix64` increment (the golden ratio in 64 bits).
const SPLITMIX64_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

/// The seed of iteration `index` (0-based) of a run with `base_seed`: output
/// `index` of a `SplitMix64` generator seeded with `base_seed`. A fixed
/// function, so a pinned base seed gives the same seeds on every toolchain.
fn iteration_seed(base_seed: u64, index: usize) -> u64 {
    let step = (index as u64).wrapping_add(1);
    crate::sim::rng::mix64(base_seed.wrapping_add(step.wrapping_mul(SPLITMIX64_GAMMA)))
}

/// Tracks iteration limits and produces deterministic seeds.
pub(crate) struct IterationManager {
    control: IterationControl,
    seeds: Vec<u64>,
    base_seed: u64,
    iteration_count: usize,
}

impl IterationManager {
    /// Create a manager with the requested control strategy and replay seeds.
    /// `base_seed` pins the base seed; `None` draws it from the wall clock.
    pub(crate) fn new(control: IterationControl, seeds: Vec<u64>, base_seed: Option<u64>) -> Self {
        Self {
            control,
            seeds,
            base_seed: base_seed.unwrap_or_else(wall_clock::default_base_seed),
            iteration_count: 0,
        }
    }

    /// Return whether another iteration fits the configured limit.
    pub(crate) fn should_continue(&self) -> bool {
        self.iteration_count < self.max_iterations()
    }

    /// Produce the current seed and advance the iteration counter.
    pub(crate) fn next_iteration(&mut self) -> u64 {
        let seed = self
            .seeds
            .get(self.iteration_count)
            .copied()
            .unwrap_or_else(|| {
                let seed = iteration_seed(self.base_seed, self.iteration_count);
                self.seeds.push(seed);
                seed
            });

        self.iteration_count += 1;
        tracing::info!(
            iteration = self.iteration_count,
            seed,
            limit = self.max_iterations(),
            "starting simulation iteration"
        );
        seed
    }

    /// Identity of this whole `run()` invocation.
    ///
    /// The base seed every generated iteration seed is derived from — fresh
    /// per invocation (wall-clock entropy) even when the caller pins explicit
    /// debug seeds, which is exactly what distinguishes one execution from
    /// another. Metric query results carry it alongside the per-iteration
    /// seed: the seed says *what* to replay, the run id says *which run* the
    /// number came from.
    pub(crate) fn run_id(&self) -> u64 {
        self.base_seed
    }

    /// Return the number of iterations started so far.
    pub(crate) fn current_iteration(&self) -> usize {
        self.iteration_count
    }

    /// Return the iteration ceiling: the fixed count, or the cap under
    /// coverage-stable control.
    pub(crate) fn max_iterations(&self) -> usize {
        match &self.control {
            IterationControl::FixedCount(count)
            | IterationControl::UntilCoverageStable {
                max_iterations: count,
                ..
            } => *count,
        }
    }

    /// Return the seeds used by completed and active iterations.
    pub(crate) fn seeds_used(&self) -> &[u64] {
        &self.seeds[..self.iteration_count]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seed sequence is `SplitMix64` of the base seed: pinned values, so a
    /// toolchain update cannot change them (#460).
    #[test]
    fn iteration_seeds_are_splitmix64_of_the_base_seed() {
        // Reference outputs of `SplitMix64` seeded with 0.
        assert_eq!(iteration_seed(0, 0), 0xe220_a839_7b1d_cdaf);
        assert_eq!(iteration_seed(0, 1), 0x6e78_9e6a_a1b9_65f4);
        assert_eq!(iteration_seed(0, 2), 0x06c4_5d18_8009_454f);

        let mut manager =
            IterationManager::new(IterationControl::FixedCount(3), Vec::new(), Some(0));
        let seeds: Vec<u64> = (0..3).map(|_| manager.next_iteration()).collect();
        assert_eq!(
            seeds,
            vec![
                0xe220_a839_7b1d_cdaf,
                0x6e78_9e6a_a1b9_65f4,
                0x06c4_5d18_8009_454f
            ]
        );
    }
}
