//! The exploration controller.
//!
//! One [`Explorer`] owns everything that decides *what to explore next*: the
//! FIFO frontier of pending jobs, the bounded per-state exemplar store, the
//! novelty bookkeeping, and the worker pool. Timeline execution stays in the
//! simulation runner — the controller only hands it jobs (recipes to replay)
//! and consumes the resulting discovery journals.
//!
//! ```text
//!                controller (this struct)
//!                        |
//!                     frontier  ◀── expansion / continuations
//!                        |
//!            +-----------+-----------+
//!            |           |           |
//!         worker      worker      worker      (≤ config.workers processes)
//!            |           |           |
//!            +-----------+-----------+
//!                        |
//!                 discovery journals
//!                        |
//!                novelty is already decided (CAS latches in the shared
//!                assertion region fire once per distinct discovery)
//!                        |
//!            productive? → expand once (branching_factor children)
//!            barren?     → the branch simply dies
//! ```
//!
//! # The exploration loop
//!
//! 1. The runner executes the root timeline for a seed and reports its
//!    journal ([`Explorer::observe_root_run`]).
//! 2. [`Explorer::explore`] pops jobs from the frontier and executes each as
//!    one full deterministic replay-plus-continuation (in a forked worker, or
//!    in-process when `workers == 0`).
//! 3. A run whose journal is non-empty (it made globally-new discoveries) is
//!    expanded **exactly once**: `branching_factor` children are enqueued,
//!    anchored at the run's latest discovery. Every discovery also registers
//!    a bounded exemplar for its semantic state.
//! 4. A run with an empty journal produced nothing new; its branch dies.
//! 5. When the frontier drains and budget remains, the controller schedules
//!    continuations from the least-visited known state (ties prefer
//!    progress-kind states, then deeper recipes) — retry pressure on the
//!    frontier without any energy accounting.
//!
//! The loop ends when the per-seed run budget is exhausted or nothing is
//! left to try. Physical process count is bounded by `1 + workers` at all
//! times; the logical exploration space is unbounded.

use std::collections::{HashMap, VecDeque};
use std::io;

use crate::journal::{self, DiscoveryEvent};
use crate::replay::Recipe;
#[cfg(unix)]
use crate::worker::WorkerExit;
use crate::worker::{self, SlotPool};

/// Maximum distinct semantic states tracked for continuation scheduling.
const MAX_TRACKED_STATES: usize = 512;

/// Maximum retained exemplars per semantic state.
///
/// Two executions can hit the same semantic bucket with very different future
/// potential (floor 4 at full health vs floor 4 about to die), so one
/// exemplar is not enough — but this stays small and bounded by design.
const MAX_EXEMPLARS_PER_STATE: usize = 3;

/// Maximum bug recipes retained per seed. Failing runs cluster around the
/// same frontier state, so a handful of reproducers is enough.
const MAX_BUG_RECIPES: usize = 4;

/// Configuration for frontier-based exploration.
#[derive(Debug, Clone)]
pub struct ExplorationConfig {
    /// Maximum concurrent worker processes. `0` runs every job in-process
    /// (sequential, fully deterministic, no fork — also the only mode on
    /// non-unix targets).
    pub workers: usize,
    /// Total timelines (root + exploration runs) per root seed.
    pub max_runs_per_seed: u64,
    /// Children enqueued when a run proves productive. One run is expanded at
    /// most once, no matter how many discoveries it made.
    pub branching_factor: u32,
    /// Cap on queued jobs; expansion stops enqueueing at this size.
    pub max_frontier: usize,
    /// Cap on recipe length (exploration depth in replay segments).
    pub max_recipe_len: usize,
}

impl Default for ExplorationConfig {
    fn default() -> Self {
        Self {
            workers: 4,
            max_runs_per_seed: 512,
            branching_factor: 4,
            max_frontier: 1024,
            max_recipe_len: 64,
        }
    }
}

/// One unit of exploration work: replay this recipe, then keep running with
/// the fresh randomness its final segment seed provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExploreJob {
    /// Replay breakpoints: `(rng_call_count, seed)` segments from the root.
    pub recipe: Recipe,
}

/// Per-seed exploration statistics.
#[derive(Debug, Clone, Default)]
pub struct ExplorationStats {
    /// Timelines executed (root + exploration runs).
    pub total_timelines: u64,
    /// Productive runs that were expanded into children.
    pub expansions: u64,
    /// Globally-new discovery events observed.
    pub discoveries: u64,
    /// Exploration runs that failed (simulation failure or worker crash).
    pub bug_found: u64,
    /// Peak number of simultaneously live worker processes.
    pub max_active_workers: usize,
    /// Peak frontier size.
    pub frontier_peak: usize,
}

/// A replayable way to reach one semantic state: replay `recipe`, and the
/// state is reached once the final segment's RNG count passes `anchor`.
#[derive(Debug, Clone)]
struct Exemplar {
    recipe: Recipe,
    anchor: u64,
}

/// Bookkeeping for one discovered semantic state (assertion site or bucket).
#[derive(Debug)]
struct StateEntry {
    state_id: u64,
    /// True when any discovery here was monotonic progress (watermark,
    /// frontier, or quality improvement) rather than plain coverage.
    progress: bool,
    /// Continuation batches scheduled from this state.
    visits: u64,
    exemplars: Vec<Exemplar>,
    /// Rotating eviction cursor once `exemplars` is full.
    next_exemplar: usize,
}

/// The exploration controller. See the [module docs](self).
pub struct Explorer {
    config: ExplorationConfig,

    // --- per-seed state, reset by `begin_seed` ---
    root_seed: u64,
    runs_started: u64,
    frontier: VecDeque<ExploreJob>,
    states: Vec<StateEntry>,
    state_index: HashMap<u64, usize>,
    continuation_round: u64,
    stats: ExplorationStats,
    bug_recipes: Vec<Recipe>,

    // --- worker pool (None = in-process mode) ---
    slots: Option<SlotPool>,
    #[cfg(unix)]
    active: HashMap<libc::pid_t, (usize, ExploreJob)>,
    free_slots: Vec<usize>,
}

impl Explorer {
    /// Create a controller: installs the discovery journal hooks, initializes
    /// the shared assertion region and sancov buffers, and allocates the
    /// worker result slots.
    ///
    /// # Errors
    ///
    /// Returns an error if shared memory allocation fails.
    pub fn new(config: ExplorationConfig) -> Result<Self, io::Error> {
        crate::init_assertions()?;
        journal::install_hooks();
        crate::sancov::init_sancov_shared()?;

        let workers = if cfg!(unix) { config.workers } else { 0 };
        let slots = if workers > 0 {
            crate::sancov::init_sancov_pool(workers);
            Some(SlotPool::new(workers)?)
        } else {
            None
        };
        // Reversed so `pop()` hands out slot 0 first (deterministic order).
        let free_slots = (0..workers).rev().collect();

        Ok(Self {
            config,
            root_seed: 0,
            runs_started: 0,
            frontier: VecDeque::new(),
            states: Vec::new(),
            state_index: HashMap::new(),
            continuation_round: 0,
            stats: ExplorationStats::default(),
            bug_recipes: Vec::new(),
            slots,
            #[cfg(unix)]
            active: HashMap::new(),
            free_slots,
        })
    }

    /// Reset per-seed state and prepare for the root run of `seed`.
    ///
    /// Cumulative novelty (the CAS latches in the shared assertion region and
    /// the sancov history) is deliberately preserved: later seeds only get
    /// exploration effort for discoveries no earlier timeline made.
    pub fn begin_seed(&mut self, seed: u64) {
        self.root_seed = seed;
        self.runs_started = 0;
        self.frontier.clear();
        self.states.clear();
        self.state_index.clear();
        self.continuation_round = 0;
        self.stats = ExplorationStats::default();
        self.bug_recipes.clear();
        journal::clear();
        crate::sancov::reset_bss_counters();
    }

    /// Consume the root run's journal and seed the frontier from it.
    ///
    /// `failed` reports whether the root run failed; root failures are
    /// visible in the main simulation report, so they are not duplicated
    /// into the exploration bug list.
    pub fn observe_root_run(&mut self, failed: bool) {
        self.runs_started += 1;
        let events = journal::take();
        // The novelty check doubles as the merge into the sancov history.
        crate::sancov::copy_counters_to_shared();
        let _ = crate::sancov::has_new_sancov_coverage();
        self.process_result(&ExploreJob { recipe: Vec::new() }, &events, failed, false);
    }

    /// Drive exploration until the frontier is exhausted or the per-seed run
    /// budget is spent.
    ///
    /// `run_one` executes a single timeline for the given job — replaying its
    /// recipe via RNG breakpoints and running the continuation to completion —
    /// and returns `true` if the run failed (found a bug). With `workers > 0`
    /// it is invoked inside a forked worker process that exits right after;
    /// with `workers == 0` it runs in-process.
    pub fn explore<F>(&mut self, mut run_one: F)
    where
        F: FnMut(&ExploreJob) -> bool,
    {
        loop {
            if self.runs_started >= self.config.max_runs_per_seed {
                break;
            }
            if self.frontier.is_empty() {
                if self.has_active_workers() {
                    self.reap_one_and_process();
                    continue;
                }
                if !self.schedule_continuations() {
                    break;
                }
                continue;
            }
            if self.slots.is_some() && self.free_slots.is_empty() {
                self.reap_one_and_process();
                continue;
            }
            let Some(job) = self.frontier.pop_front() else {
                continue;
            };
            self.runs_started += 1;
            self.run_job(job, &mut run_one);
        }
        while self.has_active_workers() {
            self.reap_one_and_process();
        }
    }

    /// Per-seed statistics.
    #[must_use]
    pub fn seed_stats(&self) -> ExplorationStats {
        self.stats.clone()
    }

    /// Bug recipes captured this seed (exploration runs that failed).
    #[must_use]
    pub fn bug_recipes(&self) -> &[Recipe] {
        &self.bug_recipes
    }

    /// Number of distinct semantic states tracked this seed.
    #[must_use]
    pub fn tracked_states(&self) -> usize {
        self.states.len()
    }

    // -----------------------------------------------------------------
    // Job execution
    // -----------------------------------------------------------------

    fn has_active_workers(&self) -> bool {
        #[cfg(unix)]
        {
            !self.active.is_empty()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    fn run_job(&mut self, job: ExploreJob, run_one: &mut impl FnMut(&ExploreJob) -> bool) {
        if self.slots.is_none() {
            self.run_job_in_process(&job, run_one);
            return;
        }
        #[cfg(unix)]
        self.spawn_worker(job, run_one);
    }

    /// In-process execution (workers == 0): sequential and fully deterministic.
    fn run_job_in_process(
        &mut self,
        job: &ExploreJob,
        run_one: &mut impl FnMut(&ExploreJob) -> bool,
    ) {
        journal::clear();
        crate::sancov::reset_bss_counters();
        let failed = run_one(job);
        let events = journal::take();
        crate::sancov::copy_counters_to_shared();
        let _ = crate::sancov::has_new_sancov_coverage();
        self.process_result(job, &events, failed, true);
    }

    /// Fork one worker for `job`. The child runs exactly one timeline, writes
    /// its journal + sancov counters into its result slot, and `_exit`s — it
    /// never returns to the exploration loop, so workers cannot recurse.
    #[cfg(unix)]
    fn spawn_worker(&mut self, job: ExploreJob, run_one: &mut impl FnMut(&ExploreJob) -> bool) {
        let slots = self.slots.as_ref().expect("worker mode has slots");
        let slot = self.free_slots.pop().expect("caller ensured a free slot");
        slots.clear_slot(slot);
        crate::sancov::clear_pool_slot(slot);

        // Safety: the controller forks between runs — single-threaded, no
        // locks held, no simulation in flight.
        let pid = unsafe { libc::fork() };
        match pid {
            -1 => {
                // Fork failed (e.g. resource limits): degrade to in-process.
                self.free_slots.push(slot);
                self.run_job_in_process(&job, run_one);
            }
            0 => {
                worker::enter_worker();
                journal::clear();
                crate::sancov::reset_bss_counters();
                crate::sancov::redirect_transfer_to_pool_slot(slot);
                let failed = run_one(&job);
                let events = journal::take();
                let slots = self.slots.as_ref().expect("worker mode has slots");
                slots.write_slot(slot, &events, journal::overflow_count());
                crate::sancov::copy_counters_to_shared();
                // Safety: _exit is always safe; skips atexit/stdio flushing,
                // which is what a forked worker wants.
                unsafe { libc::_exit(if failed { 42 } else { 0 }) }
            }
            child_pid => {
                self.active.insert(child_pid, (slot, job));
                self.stats.max_active_workers =
                    self.stats.max_active_workers.max(self.active.len());
            }
        }
    }

    /// Reap one finished worker and feed its observations to the controller.
    fn reap_one_and_process(&mut self) {
        #[cfg(unix)]
        {
            let Some((pid, status)) = worker::wait_any() else {
                // No children left (ECHILD): drop any stale bookkeeping.
                for (_, (slot, _)) in self.active.drain() {
                    self.free_slots.push(slot);
                }
                return;
            };
            let Some((slot, job)) = self.active.remove(&pid) else {
                return;
            };
            let slots = self.slots.as_ref().expect("worker mode has slots");
            let events = slots.read_slot(slot);
            // Novelty check doubles as merge into the sancov history.
            let _ = crate::sancov::has_new_pool_coverage(slot);
            self.free_slots.push(slot);

            let exit = worker::classify_exit(status);
            if exit == WorkerExit::Crashed {
                eprintln!(
                    "[explorer] worker crashed (recipe: {})",
                    crate::replay::format_timeline(&job.recipe)
                );
            }
            let failed = exit != WorkerExit::Ok;
            self.process_result(&job, &events, failed, true);
        }
    }

    // -----------------------------------------------------------------
    // Exploration policy
    // -----------------------------------------------------------------

    /// Consume one run's observations: account stats, register exemplars,
    /// and expand the run at most once if it was productive.
    fn process_result(
        &mut self,
        job: &ExploreJob,
        events: &[DiscoveryEvent],
        failed: bool,
        from_exploration: bool,
    ) {
        self.stats.total_timelines += 1;
        self.stats.discoveries += events.len() as u64;

        if failed && from_exploration {
            self.stats.bug_found += 1;
            if !job.recipe.is_empty() && self.bug_recipes.len() < MAX_BUG_RECIPES {
                self.bug_recipes.push(job.recipe.clone());
            }
        }

        // Barren run: nothing globally new — the branch dies here.
        let Some(anchor_event) = events.iter().max_by_key(|e| e.call_count) else {
            return;
        };
        let anchor_event = *anchor_event;

        for event in events {
            self.register_state(&job.recipe, event);
        }

        // One expansion per productive run, anchored at the latest discovery:
        // children replay through every state this run reached and diverge
        // just past the deepest one.
        if job.recipe.len() < self.config.max_recipe_len {
            self.stats.expansions += 1;
            self.expand_from(
                &job.recipe,
                anchor_event.call_count,
                anchor_event.state_id,
                0,
            );
            if let Some(&idx) = self.state_index.get(&anchor_event.state_id) {
                self.states[idx].visits += 1;
            }
        }
    }

    /// Record a bounded exemplar for the event's semantic state.
    fn register_state(&mut self, recipe: &Recipe, event: &DiscoveryEvent) {
        let idx = if let Some(&idx) = self.state_index.get(&event.state_id) {
            idx
        } else {
            if self.states.len() >= MAX_TRACKED_STATES {
                return;
            }
            let idx = self.states.len();
            self.states.push(StateEntry {
                state_id: event.state_id,
                progress: false,
                visits: 0,
                exemplars: Vec::new(),
                next_exemplar: 0,
            });
            self.state_index.insert(event.state_id, idx);
            idx
        };
        let state = &mut self.states[idx];
        state.progress |= event.is_progress();
        let exemplar = Exemplar {
            recipe: recipe.clone(),
            anchor: event.call_count,
        };
        if state.exemplars.len() < MAX_EXEMPLARS_PER_STATE {
            state.exemplars.push(exemplar);
        } else {
            // Evict the oldest exemplar. Re-discoveries of a known state only
            // arrive through watermark/quality improvements, so recency
            // correlates with better starting points (e.g. "floor 4 at full
            // health" replaces "floor 4 about to die").
            state.exemplars[state.next_exemplar % MAX_EXEMPLARS_PER_STATE] = exemplar;
            state.next_exemplar = state.next_exemplar.wrapping_add(1);
        }
    }

    /// Enqueue `branching_factor` children extending `base` at `anchor`.
    fn expand_from(&mut self, base: &Recipe, anchor: u64, state_id: u64, round: u64) {
        let base_seed = base.last().map_or(self.root_seed, |&(_, seed)| seed);
        for child in 0..self.config.branching_factor {
            if self.frontier.len() >= self.config.max_frontier {
                break;
            }
            let seed = derive_seed(self.root_seed, base_seed, anchor, state_id, round, child);
            let mut recipe = base.clone();
            recipe.push((anchor, seed));
            self.frontier.push_back(ExploreJob { recipe });
        }
        self.stats.frontier_peak = self.stats.frontier_peak.max(self.frontier.len());
    }

    /// Frontier is empty but budget remains: schedule a fresh continuation
    /// batch from the most promising known state. Returns `false` when there
    /// is nothing left to try.
    fn schedule_continuations(&mut self) -> bool {
        let max_len = self.config.max_recipe_len;
        let pick = self
            .states
            .iter()
            .enumerate()
            .filter(|(_, s)| s.exemplars.iter().any(|e| e.recipe.len() < max_len))
            .min_by_key(|(idx, s)| {
                let deepest = s
                    .exemplars
                    .iter()
                    .map(|e| e.recipe.len())
                    .max()
                    .unwrap_or(0);
                // Depth-weighted visit count: a state whose exemplars sit N
                // replay segments deep gets ~N+1 times the continuation
                // budget of a shallow one, so effort concentrates on the
                // frontier instead of spreading uniformly. Ties prefer
                // progress states, then deeper recipes, then registration
                // order (deterministic).
                let depth = u64::try_from(deepest).unwrap_or(u64::MAX);
                (
                    s.visits / (depth + 1),
                    u8::from(!s.progress),
                    usize::MAX - deepest,
                    *idx,
                )
            })
            .map(|(idx, _)| idx);
        let Some(idx) = pick else {
            return false;
        };

        self.continuation_round += 1;
        let (state_id, exemplar) = {
            let state = &mut self.states[idx];
            state.visits += 1;
            let eligible: Vec<&Exemplar> = state
                .exemplars
                .iter()
                .filter(|e| e.recipe.len() < max_len)
                .collect();
            // Rotate through this state's exemplars across visits.
            let chosen = usize::try_from(self.continuation_round).unwrap_or(usize::MAX)
                % eligible.len().max(1);
            (state.state_id, (*eligible[chosen]).clone())
        };
        self.expand_from(
            &exemplar.recipe,
            exemplar.anchor,
            state_id,
            self.continuation_round,
        );
        !self.frontier.is_empty()
    }
}

impl Drop for Explorer {
    fn drop(&mut self) {
        crate::sancov::cleanup_sancov_shared();
    }
}

/// Derive a child continuation seed deterministically from its coordinates.
fn derive_seed(
    root_seed: u64,
    base_seed: u64,
    anchor: u64,
    state_id: u64,
    round: u64,
    child: u32,
) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for value in [
        root_seed,
        base_seed,
        anchor,
        state_id,
        round,
        u64::from(child),
    ] {
        for byte in value.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_seed_deterministic_and_distinct() {
        let a = derive_seed(1, 2, 3, 4, 0, 0);
        assert_eq!(a, derive_seed(1, 2, 3, 4, 0, 0));
        assert_ne!(a, derive_seed(1, 2, 3, 4, 0, 1));
        assert_ne!(a, derive_seed(1, 2, 3, 4, 1, 0));
        assert_ne!(a, derive_seed(1, 2, 3, 5, 0, 0));
        assert_ne!(a, derive_seed(2, 2, 3, 4, 0, 0));
    }

    #[test]
    fn default_config_is_bounded() {
        let config = ExplorationConfig::default();
        assert!(config.workers >= 1);
        assert!(config.max_runs_per_seed > 0);
        assert!(config.branching_factor > 0);
        assert!(config.max_frontier > 0);
        assert!(config.max_recipe_len > 0);
    }
}
