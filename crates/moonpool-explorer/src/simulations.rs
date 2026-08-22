//! Self-contained exploration scenarios for the explorer's own testing.
//!
//! These exercise the full controller loop — frontier, expansion,
//! continuations, worker pool, bug recipes — against a synthetic
//! deterministic "simulation": a tiny counted xorshift RNG with the same
//! breakpoint-replay semantics as moonpool-sim's tracked RNG. They are used
//! by the `sim-frontier-explore` binary (which runs under sancov
//! instrumentation on Linux and macOS CI) and by the crate's integration
//! tests.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt;

use crate::{ExplorationConfig, ExplorationStats, Explorer, Recipe};

// ---------------------------------------------------------------------------
// Minimal counted RNG with breakpoint replay (mirrors moonpool-sim's rng.rs)
// ---------------------------------------------------------------------------

thread_local! {
    static RNG_STATE: Cell<u64> = const { Cell::new(1) };
    static CALL_COUNT: Cell<u64> = const { Cell::new(0) };
    static BREAKPOINTS: RefCell<VecDeque<(u64, u64)>> = const { RefCell::new(VecDeque::new()) };
}

/// Current RNG call count (within the active replay segment).
#[must_use]
pub fn rng_count() -> u64 {
    CALL_COUNT.with(Cell::get)
}

/// Reset the RNG to `seed` with an empty breakpoint queue.
pub fn rng_reset(seed: u64) {
    // xorshift64 requires non-zero state.
    RNG_STATE.with(|c| c.set(if seed == 0 { 1 } else { seed }));
    CALL_COUNT.with(|c| c.set(0));
    BREAKPOINTS.with(|b| b.borrow_mut().clear());
}

/// Install replay breakpoints (the recipe of the timeline to reproduce).
pub fn rng_set_breakpoints(recipe: Recipe) {
    BREAKPOINTS.with(|b| *b.borrow_mut() = VecDeque::from(recipe));
}

/// Draw the next value: counts the call and honors pending breakpoints with
/// the same "count exceeds target → reseed, count restarts at 1" contract as
/// moonpool-sim.
#[must_use]
pub fn rng_next() -> u64 {
    CALL_COUNT.with(|c| c.set(c.get() + 1));
    BREAKPOINTS.with(|b| {
        let mut breakpoints = b.borrow_mut();
        while let Some(&(target, new_seed)) = breakpoints.front() {
            if CALL_COUNT.with(Cell::get) > target {
                breakpoints.pop_front();
                RNG_STATE.with(|c| c.set(if new_seed == 0 { 1 } else { new_seed }));
                CALL_COUNT.with(|c| c.set(1));
            } else {
                break;
            }
        }
    });
    RNG_STATE.with(|c| {
        let mut s = c.get();
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        c.set(s);
        s
    })
}

/// Uniform draw in `0..divisor`.
#[must_use]
pub fn rng_below(divisor: u64) -> u64 {
    rng_next() % divisor
}

// ---------------------------------------------------------------------------
// The floor-ladder scenario
// ---------------------------------------------------------------------------

/// Number of floors in the ladder; reaching the last one is the "bug".
pub const LADDER_FLOORS: u64 = 8;

/// Per-floor advance probability is `1/LADDER_GATE`.
pub const LADDER_GATE: u64 = 6;

/// One run of the ladder: at each floor, a `1/LADDER_GATE` gate decides
/// whether the run advances. Each floor reached fires an
/// `assert_sometimes_each!`-style bucket (the exploration anchor). Returns
/// `true` when the final floor was reached — the planted "bug".
///
/// Brute-force probability of the bug per run: `LADDER_GATE^-(LADDER_FLOORS)`
/// ≈ 6e-7. The explorer climbs the ladder by anchoring continuations at the
/// deepest floor reached so far.
fn run_ladder() -> bool {
    // Entry mark: fires on the very first run, giving the controller an
    // anchor at RNG count 0 — continuations from it are fresh-seed restarts.
    moonpool_assertions::assertion_bool(
        moonpool_assertions::AssertKind::Sometimes,
        true,
        true,
        "ladder entry",
    );
    let mut floor: u64 = 0;
    while floor < LADDER_FLOORS {
        if rng_below(LADDER_GATE) != 0 {
            break;
        }
        floor += 1;
        moonpool_assertions::assertion_sometimes_each(
            "ladder floor",
            &[("floor", i64::try_from(floor).unwrap_or(i64::MAX))],
            &[],
        );
    }
    floor >= LADDER_FLOORS
}

/// Outcome of a [`run_frontier_ladder`] scenario.
#[derive(Debug, Clone)]
pub struct LadderOutcome {
    /// Controller statistics for the (single) seed.
    pub stats: ExplorationStats,
    /// Bug recipes captured (runs that reached the final floor).
    pub bug_recipes: Vec<Recipe>,
    /// Deepest floor bucket discovered.
    pub deepest_floor: i64,
    /// Distinct semantic states tracked by the controller.
    pub tracked_states: usize,
}

/// Errors from explorer scenarios.
#[derive(Debug)]
pub enum ScenarioError {
    /// Controller initialization failed.
    Init(std::io::Error),
}

impl fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Init(e) => write!(f, "explorer init failed: {e}"),
        }
    }
}

impl std::error::Error for ScenarioError {}

/// Run the floor-ladder scenario under a fresh [`Explorer`].
///
/// Deterministic for `config.workers == 0`; with workers the search order
/// depends on reap order but every recipe remains individually reproducible.
///
/// # Errors
///
/// Returns [`ScenarioError::Init`] when controller initialization fails.
pub fn run_frontier_ladder(
    root_seed: u64,
    config: ExplorationConfig,
) -> Result<LadderOutcome, ScenarioError> {
    crate::set_rng_count_hook(rng_count);

    let mut explorer = Explorer::new(config).map_err(ScenarioError::Init)?;
    explorer.begin_seed(root_seed);

    // Root run.
    rng_reset(root_seed);
    let root_failed = run_ladder();
    explorer.observe_root_run(root_failed);

    // Exploration runs.
    explorer.explore(|job| {
        rng_reset(root_seed);
        rng_set_breakpoints(job.recipe.clone());
        run_ladder()
    });

    let deepest_floor = crate::each_bucket_read_all()
        .iter()
        .filter(|b| b.msg_str() == "ladder floor")
        .map(|b| b.key_values[0])
        .max()
        .unwrap_or(0);

    let outcome = LadderOutcome {
        stats: explorer.seed_stats(),
        bug_recipes: explorer.bug_recipes().to_vec(),
        deepest_floor,
        tracked_states: explorer.tracked_states(),
    };
    drop(explorer);
    crate::cleanup_assertions();
    moonpool_assertions::clear_discovery_hooks();
    Ok(outcome)
}

/// Replay a recipe against the ladder and report whether it reproduces the
/// bug (reaches the final floor). Runs with pure accounting (no region, no
/// hooks), so the assertion calls inside the ladder are no-ops.
#[must_use]
pub fn ladder_replay_reproduces(root_seed: u64, recipe: &Recipe) -> bool {
    rng_reset(root_seed);
    rng_set_breakpoints(recipe.clone());
    run_ladder()
}
