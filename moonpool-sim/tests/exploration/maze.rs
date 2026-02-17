//! Maze workload for fork-based exploration testing.
//!
//! 4 locks, each behind 5 nested probability gates (P=0.05).
//! Without exploration: P(all 4 locks) = (0.05^5)^4 ~ impossible.
//! With exploration: finds all paths in seconds via fork amplification.
//!
//! Ported from `/home/pierrez/workspace/rust/Claude-fork-testing/maze-explorer/src/maze.rs`.

use async_trait::async_trait;
use moonpool_sim::{
    ExplorationConfig, SimContext, SimulationBuilder, SimulationReport, SimulationResult, Workload,
};

// Gate probability — each nested `if` has this chance of passing.
const GATE_P: f64 = 0.05;
const DEFAULT_MAX_STEPS: u64 = 500;

/// Result of a single step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepResult {
    Continue,
    BugFound,
    Done,
}

/// A program with deeply nested random branches and layered locks.
///
/// 4 locks (bits 0-3). Each lock requires passing through 5 nested random
/// gates, each with P=0.05. The bug triggers only when all 4 locks are open.
struct Maze {
    step_count: u64,
    max_steps: u64,
    locks: u8,
}

impl Maze {
    fn new(max_steps: u64) -> Self {
        Maze {
            step_count: 0,
            max_steps,
            locks: 0,
        }
    }

    fn all_locks_open(&self) -> bool {
        self.locks & 0b1111 == 0b1111
    }

    fn random_bool(p: f64) -> bool {
        moonpool_sim::sim_random::<f64>() < p
    }

    fn step(&mut self) -> StepResult {
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

    fn run(&mut self) -> StepResult {
        loop {
            match self.step() {
                StepResult::Continue => {}
                result => return result,
            }
        }
    }
}

/// Workload that runs the maze and reports bugs via assertion.
struct MazeWorkload {
    max_steps: u64,
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

/// Helper to run a simulation and return the report.
fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move { builder.run().await })
}

/// Maze exploration test with fork-based amplification.
///
/// With 4 locks * 5 gates at P=0.05, brute force is essentially impossible.
/// Fork-based exploration should find all paths efficiently.
#[test]
fn slow_simulation_maze() {
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .enable_exploration(ExplorationConfig {
                max_depth: 30,
                timelines_per_split: 4,
                global_energy: 50_000,
                adaptive: Some(moonpool_sim::AdaptiveConfig {
                    batch_size: 20,
                    min_timelines: 100,
                    max_timelines: 200,
                    per_mark_energy: 1_000,
                }),
            })
            .workload(MazeWorkload {
                max_steps: DEFAULT_MAX_STEPS,
            }),
    );

    // Parent run should succeed (bugs only found in forked children)
    assert_eq!(report.successful_runs, 1);

    let exp = report.exploration.expect("exploration report missing");
    assert!(exp.total_timelines > 0, "expected forked timelines, got 0");
    assert!(exp.bugs_found > 0, "exploration should have found the bug");
}

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
    let report = run_simulation(
        SimulationBuilder::new()
            .set_iterations(1)
            .set_debug_seeds(vec![12345])
            .enable_exploration(ExplorationConfig {
                max_depth: 30,
                timelines_per_split: 4,
                global_energy: 50_000,
                adaptive: Some(moonpool_sim::AdaptiveConfig {
                    batch_size: 20,
                    min_timelines: 100,
                    max_timelines: 200,
                    per_mark_energy: 1_000,
                }),
            })
            .workload(MazeWorkload {
                max_steps: DEFAULT_MAX_STEPS,
            }),
    );

    assert_eq!(report.successful_runs, 1);

    let exp = report.exploration.expect("exploration report missing");
    assert!(exp.bugs_found > 0, "exploration should have found the bug");
    let recipe = exp.bug_recipe.expect("bug recipe should be captured");
    let initial_seed = report.seeds_used[0];

    // Phase 2: Simulate what a developer does — format and parse the recipe.
    // In practice the recipe is printed to logs; the developer copies it to replay.
    let timeline_str = moonpool_sim::format_timeline(&recipe);
    eprintln!(
        "Replaying bug: seed={}, recipe={}",
        initial_seed, timeline_str
    );
    let parsed_recipe =
        moonpool_sim::parse_timeline(&timeline_str).expect("recipe should round-trip");
    assert_eq!(recipe, parsed_recipe);

    // Phase 3: Replay — same seed, breakpoints from recipe, no exploration.
    // The RNG follows the exact same path the bug-finding child took.
    moonpool_sim::reset_sim_rng();
    moonpool_sim::set_sim_seed(initial_seed);
    moonpool_sim::set_rng_breakpoints(parsed_recipe);

    let mut maze = Maze::new(DEFAULT_MAX_STEPS);
    let result = maze.run();

    assert_eq!(
        result,
        StepResult::BugFound,
        "replaying the recipe should reproduce the bug"
    );
}
