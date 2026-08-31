//! Iteration control and deterministic seed generation.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::builder::IterationControl;
use super::wall_clock::{self, Instant};

/// Tracks iteration limits and produces deterministic seeds.
pub(crate) struct IterationManager {
    control: IterationControl,
    seeds: Vec<u64>,
    base_seed: u64,
    iteration_count: usize,
    start_time: Instant,
}

impl IterationManager {
    /// Create a manager with the requested control strategy and replay seeds.
    pub(crate) fn new(control: IterationControl, seeds: Vec<u64>) -> Self {
        Self {
            control,
            seeds,
            base_seed: wall_clock::default_base_seed(),
            iteration_count: 0,
            start_time: Instant::now(),
        }
    }

    /// Return whether another iteration fits the configured limit.
    pub(crate) fn should_continue(&self) -> bool {
        match &self.control {
            IterationControl::FixedCount(count)
            | IterationControl::UntilCoverageStable {
                max_iterations: count,
                ..
            } => self.iteration_count < *count,
            IterationControl::TimeLimit(duration) => self.start_time.elapsed() < *duration,
        }
    }

    /// Produce the current seed and advance the iteration counter.
    pub(crate) fn next_iteration(&mut self) -> u64 {
        let seed = self
            .seeds
            .get(self.iteration_count)
            .copied()
            .unwrap_or_else(|| {
                let mut hasher = DefaultHasher::new();
                self.base_seed.hash(&mut hasher);
                self.iteration_count.hash(&mut hasher);
                let seed = hasher.finish();
                self.seeds.push(seed);
                seed
            });

        self.iteration_count += 1;
        tracing::info!(
            iteration = self.iteration_count,
            seed,
            limit = self.max_iterations().unwrap_or_default(),
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

    /// Return the fixed iteration limit, if this is not time-based.
    pub(crate) fn max_iterations(&self) -> Option<usize> {
        match &self.control {
            IterationControl::FixedCount(count)
            | IterationControl::UntilCoverageStable {
                max_iterations: count,
                ..
            } => Some(*count),
            IterationControl::TimeLimit(_) => None,
        }
    }

    /// Return the seeds used by completed and active iterations.
    pub(crate) fn seeds_used(&self) -> &[u64] {
        &self.seeds[..self.iteration_count]
    }
}
