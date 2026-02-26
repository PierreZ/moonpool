//! Maze workload for fork-based exploration testing.
//!
//! 4 locks, each behind 5 nested probability gates (P=0.05).
//! Without exploration: P(all 4 locks) = (0.05^5)^4 ~ impossible.
//! With exploration: finds all paths in seconds via fork amplification.
//!
//! Ported from `/home/pierrez/workspace/rust/Claude-fork-testing/maze-explorer/src/maze.rs`.

use crate::{SimContext, SimulationResult, Workload};
use async_trait::async_trait;

/// Gate probability -- each nested `if` has this chance of passing.
pub const GATE_P: f64 = 0.05;
/// Default maximum steps per run.
pub const DEFAULT_MAX_STEPS: u64 = 500;

/// Result of a single step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepResult {
    /// Keep running.
    Continue,
    /// Bug found (all locks opened).
    BugFound,
    /// Reached step limit.
    Done,
}

/// A program with deeply nested random branches and layered locks.
///
/// 4 locks (bits 0-3). Each lock requires passing through 5 nested random
/// gates, each with P=0.05. The bug triggers only when all 4 locks are open.
pub struct Maze {
    step_count: u64,
    max_steps: u64,
    locks: u8,
}

impl Maze {
    /// Create a new maze with the given step limit.
    pub fn new(max_steps: u64) -> Self {
        Maze {
            step_count: 0,
            max_steps,
            locks: 0,
        }
    }

    /// Returns true when all 4 locks are open.
    pub fn all_locks_open(&self) -> bool {
        self.locks & 0b1111 == 0b1111
    }

    fn random_bool(p: f64) -> bool {
        crate::sim_random::<f64>() < p
    }

    /// Execute one step of the maze.
    pub fn step(&mut self) -> StepResult {
        self.step_count += 1;

        // Consume one random value per step for state mixing.
        let _: u64 = crate::sim_random();

        let locks_open = self.locks.count_ones() as i64;

        // --- Lock 0: 5 nested gates (independent) ---
        if self.locks & 1 == 0 {
            crate::assert_sometimes_each!(
                "gate",
                [("lock", 0i64), ("depth", 1i64), ("locks", locks_open)]
            );
            if Self::random_bool(GATE_P) {
                crate::assert_sometimes_each!(
                    "gate",
                    [("lock", 0i64), ("depth", 2i64), ("locks", locks_open)]
                );
                if Self::random_bool(GATE_P) {
                    crate::assert_sometimes_each!(
                        "gate",
                        [("lock", 0i64), ("depth", 3i64), ("locks", locks_open)]
                    );
                    if Self::random_bool(GATE_P) {
                        crate::assert_sometimes_each!(
                            "gate",
                            [("lock", 0i64), ("depth", 4i64), ("locks", locks_open)]
                        );
                        if Self::random_bool(GATE_P) {
                            crate::assert_sometimes_each!(
                                "gate",
                                [("lock", 0i64), ("depth", 5i64), ("locks", locks_open)]
                            );
                            if Self::random_bool(GATE_P) {
                                self.locks |= 1;
                                crate::assert_sometimes_each!(
                                    "lock opens",
                                    [("lock", 0i64), ("locks", locks_open)]
                                );
                            }
                        }
                    }
                }
            }
        }

        // --- Lock 1: 5 nested gates (requires lock 0) ---
        let locks_open = self.locks.count_ones() as i64;
        if self.locks & 1 != 0 && self.locks & 2 == 0 {
            crate::assert_sometimes_each!(
                "gate",
                [("lock", 1i64), ("depth", 1i64), ("locks", locks_open)]
            );
            if Self::random_bool(GATE_P) {
                crate::assert_sometimes_each!(
                    "gate",
                    [("lock", 1i64), ("depth", 2i64), ("locks", locks_open)]
                );
                if Self::random_bool(GATE_P) {
                    crate::assert_sometimes_each!(
                        "gate",
                        [("lock", 1i64), ("depth", 3i64), ("locks", locks_open)]
                    );
                    if Self::random_bool(GATE_P) {
                        crate::assert_sometimes_each!(
                            "gate",
                            [("lock", 1i64), ("depth", 4i64), ("locks", locks_open)]
                        );
                        if Self::random_bool(GATE_P) {
                            crate::assert_sometimes_each!(
                                "gate",
                                [("lock", 1i64), ("depth", 5i64), ("locks", locks_open)]
                            );
                            if Self::random_bool(GATE_P) {
                                self.locks |= 2;
                                crate::assert_sometimes_each!(
                                    "lock opens",
                                    [("lock", 1i64), ("locks", locks_open)]
                                );
                            }
                        }
                    }
                }
            }
        }

        // --- Lock 2: 5 nested gates (independent) ---
        let locks_open = self.locks.count_ones() as i64;
        if self.locks & 4 == 0 {
            crate::assert_sometimes_each!(
                "gate",
                [("lock", 2i64), ("depth", 1i64), ("locks", locks_open)]
            );
            if Self::random_bool(GATE_P) {
                crate::assert_sometimes_each!(
                    "gate",
                    [("lock", 2i64), ("depth", 2i64), ("locks", locks_open)]
                );
                if Self::random_bool(GATE_P) {
                    crate::assert_sometimes_each!(
                        "gate",
                        [("lock", 2i64), ("depth", 3i64), ("locks", locks_open)]
                    );
                    if Self::random_bool(GATE_P) {
                        crate::assert_sometimes_each!(
                            "gate",
                            [("lock", 2i64), ("depth", 4i64), ("locks", locks_open)]
                        );
                        if Self::random_bool(GATE_P) {
                            crate::assert_sometimes_each!(
                                "gate",
                                [("lock", 2i64), ("depth", 5i64), ("locks", locks_open)]
                            );
                            if Self::random_bool(GATE_P) {
                                self.locks |= 4;
                                crate::assert_sometimes_each!(
                                    "lock opens",
                                    [("lock", 2i64), ("locks", locks_open)]
                                );
                            }
                        }
                    }
                }
            }
        }

        // --- Lock 3: 5 nested gates (requires lock 0 AND lock 2) ---
        let locks_open = self.locks.count_ones() as i64;
        if self.locks & 1 != 0 && self.locks & 4 != 0 && self.locks & 8 == 0 {
            crate::assert_sometimes_each!(
                "gate",
                [("lock", 3i64), ("depth", 1i64), ("locks", locks_open)]
            );
            if Self::random_bool(GATE_P) {
                crate::assert_sometimes_each!(
                    "gate",
                    [("lock", 3i64), ("depth", 2i64), ("locks", locks_open)]
                );
                if Self::random_bool(GATE_P) {
                    crate::assert_sometimes_each!(
                        "gate",
                        [("lock", 3i64), ("depth", 3i64), ("locks", locks_open)]
                    );
                    if Self::random_bool(GATE_P) {
                        crate::assert_sometimes_each!(
                            "gate",
                            [("lock", 3i64), ("depth", 4i64), ("locks", locks_open)]
                        );
                        if Self::random_bool(GATE_P) {
                            crate::assert_sometimes_each!(
                                "gate",
                                [("lock", 3i64), ("depth", 5i64), ("locks", locks_open)]
                            );
                            if Self::random_bool(GATE_P) {
                                self.locks |= 8;
                                crate::assert_sometimes_each!(
                                    "lock opens",
                                    [("lock", 3i64), ("locks", locks_open)]
                                );
                            }
                        }
                    }
                }
            }
        }

        // --- Check for the bug ---
        if self.all_locks_open() {
            crate::assert_sometimes!(true, "all locks open");
            return StepResult::BugFound;
        }

        if self.step_count >= self.max_steps {
            return StepResult::Done;
        }

        StepResult::Continue
    }

    /// Run the maze until bug or step limit.
    pub fn run(&mut self) -> StepResult {
        loop {
            match self.step() {
                StepResult::Continue => {}
                result => return result,
            }
        }
    }
}

/// Workload that runs the maze and reports bugs via assertion.
pub struct MazeWorkload {
    /// Maximum steps per run.
    pub max_steps: u64,
}

impl Default for MazeWorkload {
    fn default() -> Self {
        MazeWorkload {
            max_steps: DEFAULT_MAX_STEPS,
        }
    }
}

#[async_trait(?Send)]
impl Workload for MazeWorkload {
    fn name(&self) -> &str {
        "maze"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        let mut maze = Maze::new(self.max_steps);
        let result = maze.run();

        crate::assert_always!(
            result != StepResult::BugFound,
            "maze bug: all 4 locks opened"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExplorationConfig, SimulationBuilder};

    /// Test that a bug found by exploration can be replayed deterministically.
    ///
    /// This demonstrates the full bug-reproduction workflow:
    /// 1. Exploration finds the bug and captures a recipe (sequence of RNG fork points)
    /// 2. The recipe is formatted as a human-readable timeline string (for logs/issue reports)
    /// 3. A developer parses the recipe string and replays it using RNG breakpoints
    /// 4. The exact same bug reproduces without any exploration or forking
    #[test]
    fn slow_simulation_maze_bug_replay() {
        // Phase 1: Run exploration to find the bug and capture the recipe.
        let report = super::super::run_simulation(
            SimulationBuilder::new()
                .set_iterations(2)
                .set_debug_seeds(vec![12345])
                .enable_exploration(ExplorationConfig {
                    max_depth: 30,
                    timelines_per_split: 4,
                    global_energy: 20_000,
                    adaptive: Some(crate::AdaptiveConfig {
                        batch_size: 20,
                        min_timelines: 60,
                        max_timelines: 150,
                        per_mark_energy: 600,
                        warm_min_timelines: Some(20),
                    }),
                    parallelism: Some(crate::Parallelism::MaxCores),
                })
                .workload(MazeWorkload {
                    max_steps: DEFAULT_MAX_STEPS,
                }),
        );

        assert_eq!(report.successful_runs, 2);

        let exp = report.exploration.expect("exploration report missing");
        assert!(exp.bugs_found > 0, "exploration should have found the bug");
        let bug = exp
            .bug_recipes
            .first()
            .expect("bug recipe should be captured");
        let initial_seed = bug.seed;

        // Phase 2: Simulate what a developer does -- format and parse the recipe.
        // In practice the recipe is printed to logs; the developer copies it to replay.
        let timeline_str = crate::format_timeline(&bug.recipe);
        eprintln!(
            "Replaying bug: seed={}, recipe={}",
            initial_seed, timeline_str
        );
        let parsed_recipe = crate::parse_timeline(&timeline_str).expect("recipe should round-trip");
        assert_eq!(bug.recipe, parsed_recipe);

        // Phase 3: Replay -- same seed, breakpoints from recipe, no exploration.
        // The RNG follows the exact same path the bug-finding child took.
        crate::reset_sim_rng();
        crate::set_sim_seed(initial_seed);
        crate::set_rng_breakpoints(parsed_recipe);

        let mut maze = Maze::new(DEFAULT_MAX_STEPS);
        let result = maze.run();

        assert_eq!(
            result,
            StepResult::BugFound,
            "replaying the recipe should reproduce the bug"
        );
    }
}
