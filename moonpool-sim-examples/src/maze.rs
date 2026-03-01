//! Maze workload for fork-based exploration testing.
//!
//! 4 locks, each behind 5 nested probability gates (P=0.05).
//! Without exploration: P(all 4 locks) = (0.05^5)^4 ~ impossible.
//! With exploration: finds all paths in seconds via fork amplification.
//!
//! Ported from `/home/pierrez/workspace/rust/Claude-fork-testing/maze-explorer/src/maze.rs`.

use async_trait::async_trait;
use moonpool_sim::{SimContext, SimulationResult, Workload};

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
        moonpool_sim::sim_random::<f64>() < p
    }

    /// Execute one step of the maze.
    pub fn step(&mut self) -> StepResult {
        self.step_count += 1;

        // Consume one random value per step for state mixing.
        let _: u64 = moonpool_sim::sim_random();

        let locks_open = self.locks.count_ones() as i64;

        // --- Lock 0: 5 nested gates (independent) ---
        if self.locks & 1 == 0 {
            moonpool_sim::assert_sometimes_each!(
                "gate",
                [("lock", 0i64), ("depth", 1i64), ("locks", locks_open)]
            );
            if Self::random_bool(GATE_P) {
                moonpool_sim::assert_sometimes_each!(
                    "gate",
                    [("lock", 0i64), ("depth", 2i64), ("locks", locks_open)]
                );
                if Self::random_bool(GATE_P) {
                    moonpool_sim::assert_sometimes_each!(
                        "gate",
                        [("lock", 0i64), ("depth", 3i64), ("locks", locks_open)]
                    );
                    if Self::random_bool(GATE_P) {
                        moonpool_sim::assert_sometimes_each!(
                            "gate",
                            [("lock", 0i64), ("depth", 4i64), ("locks", locks_open)]
                        );
                        if Self::random_bool(GATE_P) {
                            moonpool_sim::assert_sometimes_each!(
                                "gate",
                                [("lock", 0i64), ("depth", 5i64), ("locks", locks_open)]
                            );
                            if Self::random_bool(GATE_P) {
                                self.locks |= 1;
                                moonpool_sim::assert_sometimes_each!(
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
            moonpool_sim::assert_sometimes_each!(
                "gate",
                [("lock", 1i64), ("depth", 1i64), ("locks", locks_open)]
            );
            if Self::random_bool(GATE_P) {
                moonpool_sim::assert_sometimes_each!(
                    "gate",
                    [("lock", 1i64), ("depth", 2i64), ("locks", locks_open)]
                );
                if Self::random_bool(GATE_P) {
                    moonpool_sim::assert_sometimes_each!(
                        "gate",
                        [("lock", 1i64), ("depth", 3i64), ("locks", locks_open)]
                    );
                    if Self::random_bool(GATE_P) {
                        moonpool_sim::assert_sometimes_each!(
                            "gate",
                            [("lock", 1i64), ("depth", 4i64), ("locks", locks_open)]
                        );
                        if Self::random_bool(GATE_P) {
                            moonpool_sim::assert_sometimes_each!(
                                "gate",
                                [("lock", 1i64), ("depth", 5i64), ("locks", locks_open)]
                            );
                            if Self::random_bool(GATE_P) {
                                self.locks |= 2;
                                moonpool_sim::assert_sometimes_each!(
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
            moonpool_sim::assert_sometimes_each!(
                "gate",
                [("lock", 2i64), ("depth", 1i64), ("locks", locks_open)]
            );
            if Self::random_bool(GATE_P) {
                moonpool_sim::assert_sometimes_each!(
                    "gate",
                    [("lock", 2i64), ("depth", 2i64), ("locks", locks_open)]
                );
                if Self::random_bool(GATE_P) {
                    moonpool_sim::assert_sometimes_each!(
                        "gate",
                        [("lock", 2i64), ("depth", 3i64), ("locks", locks_open)]
                    );
                    if Self::random_bool(GATE_P) {
                        moonpool_sim::assert_sometimes_each!(
                            "gate",
                            [("lock", 2i64), ("depth", 4i64), ("locks", locks_open)]
                        );
                        if Self::random_bool(GATE_P) {
                            moonpool_sim::assert_sometimes_each!(
                                "gate",
                                [("lock", 2i64), ("depth", 5i64), ("locks", locks_open)]
                            );
                            if Self::random_bool(GATE_P) {
                                self.locks |= 4;
                                moonpool_sim::assert_sometimes_each!(
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
            moonpool_sim::assert_sometimes_each!(
                "gate",
                [("lock", 3i64), ("depth", 1i64), ("locks", locks_open)]
            );
            if Self::random_bool(GATE_P) {
                moonpool_sim::assert_sometimes_each!(
                    "gate",
                    [("lock", 3i64), ("depth", 2i64), ("locks", locks_open)]
                );
                if Self::random_bool(GATE_P) {
                    moonpool_sim::assert_sometimes_each!(
                        "gate",
                        [("lock", 3i64), ("depth", 3i64), ("locks", locks_open)]
                    );
                    if Self::random_bool(GATE_P) {
                        moonpool_sim::assert_sometimes_each!(
                            "gate",
                            [("lock", 3i64), ("depth", 4i64), ("locks", locks_open)]
                        );
                        if Self::random_bool(GATE_P) {
                            moonpool_sim::assert_sometimes_each!(
                                "gate",
                                [("lock", 3i64), ("depth", 5i64), ("locks", locks_open)]
                            );
                            if Self::random_bool(GATE_P) {
                                self.locks |= 8;
                                moonpool_sim::assert_sometimes_each!(
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
            moonpool_sim::assert_sometimes!(true, "all locks open");
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

        moonpool_sim::assert_always!(
            result != StepResult::BugFound,
            "maze bug: all 4 locks opened"
        );

        Ok(())
    }
}
