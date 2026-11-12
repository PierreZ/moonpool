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
//!    anchored at its highest-priority discovery (latest breaks ties). Every
//!    discovery also registers a bounded exemplar for its semantic state.
//! 4. A run with an empty journal produced nothing new; its branch dies.
//! 5. When the frontier drains and budget remains, the controller schedules
//!    continuations from the least-visited known state (ties prefer
//!    progress-kind states, then deeper recipes) — retry pressure on the
//!    frontier without any energy accounting.
//!
//! The loop ends when the per-seed run budget is exhausted or nothing is
//! left to try. Physical process count is bounded by `1 + workers` at all
//! times; the logical exploration space is unbounded.

use std::collections::{BTreeMap, VecDeque};
use std::io;
#[cfg(unix)]
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

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

/// Default wall-clock ceiling for one forked worker's timeline.
///
/// A worker stuck outside the simulation (a tight loop in user code, a
/// blocking syscall) is invisible to the in-simulation stall guards; past this
/// ceiling the controller kills it and records the run as a crash, with a
/// diagnostic, instead of blocking until the CI job's own timeout. Generous on
/// purpose: a timeline is normally milliseconds to seconds.
pub const DEFAULT_WORKER_TIMEOUT: Duration = Duration::from_mins(5);

/// Polls of a busy worker pool that only yield before the controller starts
/// sleeping between polls: short timelines finish within the yield phase, so
/// the common case pays no sleep latency.
const SPIN_POLLS: u32 = 2048;

/// First sleep between polls once [`SPIN_POLLS`] ran out; each further
/// empty poll doubles it, up to [`MAX_POLL_SLEEP`].
const MIN_POLL_SLEEP: Duration = Duration::from_micros(10);

/// Longest sleep between polls: a long timeline is noticed within this.
const MAX_POLL_SLEEP: Duration = Duration::from_millis(1);

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
            // Raw fork is opt-in: in-process mode is deterministic and safe in
            // test harnesses or applications that may already have threads.
            workers: 0,
            max_runs_per_seed: 512,
            branching_factor: 4,
            max_frontier: 1024,
            max_recipe_len: 64,
        }
    }
}

impl ExplorationConfig {
    /// Validate that the run budget and policy bounds are nonzero.
    /// `workers == 0` remains valid and selects deterministic, in-process
    /// execution.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] with the invalid field name when
    /// a run, branching, frontier, or recipe bound is zero.
    pub fn validate(&self) -> Result<(), io::Error> {
        for (field, value) in [
            ("max_runs_per_seed", self.max_runs_per_seed),
            ("branching_factor", u64::from(self.branching_factor)),
            (
                "max_frontier",
                u64::try_from(self.max_frontier).unwrap_or(u64::MAX),
            ),
            (
                "max_recipe_len",
                u64::try_from(self.max_recipe_len).unwrap_or(u64::MAX),
            ),
        ] {
            if value == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("exploration config field `{field}` must be greater than zero"),
                ));
            }
        }
        Ok(())
    }
}

/// One unit of exploration work: replay this recipe, then keep running with
/// the fresh randomness its final segment seed provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExploreJob {
    /// Replay breakpoints: `(rng_call_count, seed)` segments from the root.
    pub recipe: Recipe,
}

/// One timeline the explorer ran, kept when
/// [`Explorer::record_timelines`] is on: the tree of an exploration, one node
/// per timeline. The parent of a timeline is the one whose recipe is this
/// recipe without its last segment; the root has an empty recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineRecord {
    /// The recipe it replayed: `(rng_call_count, seed)` segments from the
    /// root. The last segment is the fork: the parent's state at that call
    /// count, then fresh randomness.
    pub recipe: Recipe,
    /// The timeline failed (a bug) or its worker crashed.
    pub failed: bool,
    /// The globally new discoveries it made, in order.
    pub discoveries: Vec<DiscoveryEvent>,
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
    /// Runs with new sancov edges and no new assertion state, expanded once
    /// at low priority.
    pub edge_expansions: u64,
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
    state_index: BTreeMap<u64, usize>,
    continuation_round: u64,
    stats: ExplorationStats,
    bug_recipes: Vec<Recipe>,
    /// Every timeline of the seed, when `record` is on.
    timelines: Vec<TimelineRecord>,
    record: bool,

    // --- worker pool (None = in-process mode) ---
    slots: Option<SlotPool>,
    /// Live workers: result slot, job, and when the worker was forked.
    #[cfg(unix)]
    active: BTreeMap<libc::pid_t, (usize, ExploreJob, Instant)>,
    free_slots: Vec<usize>,
    /// Wall-clock ceiling for one worker (see [`DEFAULT_WORKER_TIMEOUT`]).
    worker_timeout: Duration,
}

impl Drop for Explorer {
    /// Kill and reap every live worker.
    ///
    /// A controller that unwinds (a panic in the policy code, an early return
    /// by the embedding harness) must not orphan workers that keep running
    /// full simulations while holding the shared assertion and sancov regions.
    ///
    /// A forked worker has a copy of the controller, with the pids of its
    /// siblings. It did not fork them, so it must not kill or reap them.
    fn drop(&mut self) {
        if worker::explorer_is_child() {
            return;
        }
        #[cfg(unix)]
        for (pid, _) in std::mem::take(&mut self.active) {
            let _ = worker::kill_worker(pid);
        }
    }
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
        config.validate()?;
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
            state_index: BTreeMap::new(),
            continuation_round: 0,
            stats: ExplorationStats::default(),
            bug_recipes: Vec::new(),
            timelines: Vec::new(),
            record: false,
            slots,
            #[cfg(unix)]
            active: BTreeMap::new(),
            free_slots,
            worker_timeout: DEFAULT_WORKER_TIMEOUT,
        })
    }

    /// Replace the per-worker wall-clock ceiling (default
    /// [`DEFAULT_WORKER_TIMEOUT`]). A worker still running past it is killed
    /// and its run recorded as a crash.
    pub fn set_worker_timeout(&mut self, timeout: Duration) {
        self.worker_timeout = timeout;
    }

    /// Keep a [`TimelineRecord`] of each timeline of a seed
    /// ([`Self::timelines`]). Off by default: a campaign of many seeds runs
    /// many timelines. With `workers > 0` the records come in the order the
    /// workers finish; with `workers == 0` the order is deterministic.
    pub fn record_timelines(&mut self, on: bool) {
        self.record = on;
    }

    /// The timelines of the current seed, in the order they finished
    /// (the root first), when [`Self::record_timelines`] is on.
    #[must_use]
    pub fn timelines(&self) -> &[TimelineRecord] {
        &self.timelines
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
        self.timelines.clear();
        begin_run();
    }

    /// Consume the root run's journal and seed the frontier from it.
    ///
    /// `failed` reports whether the root run failed; root failures are
    /// visible in the main simulation report, so they are not duplicated
    /// into the exploration bug list.
    pub fn observe_root_run(&mut self, failed: bool) {
        self.runs_started += 1;
        self.finish_in_process_run(&ExploreJob { recipe: Vec::new() }, failed, false);
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
        begin_run();
        let failed = run_one(job);
        self.finish_in_process_run(job, failed, true);
    }

    /// Collect an in-process run's journal and coverage, then consume them.
    fn finish_in_process_run(&mut self, job: &ExploreJob, failed: bool, from_exploration: bool) {
        let events = journal::take();
        let end_count = journal::rng_count();
        // The novelty check doubles as the merge into the sancov history.
        crate::sancov::copy_counters_to_shared();
        let new_edges = crate::sancov::has_new_sancov_coverage();
        let run = RunResult {
            events: &events,
            failed,
            new_edges,
            end_count,
        };
        self.process_result(job, &run, from_exploration);
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
                begin_run();
                crate::sancov::redirect_transfer_to_pool_slot(slot);
                // A panic must not unwind out of the child: it would run the
                // caller's code (the rest of `explore`, a test harness) in a
                // process that must only run this one timeline. The panic is
                // reported as a crash, and the controller records the recipe
                // of the job.
                let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| run_one(&job)));
                let events = journal::take();
                let slots = self.slots.as_ref().expect("worker mode has slots");
                slots.write_slot(slot, &events, journal::rng_count());
                crate::sancov::copy_counters_to_shared();
                let code = match outcome {
                    Ok(false) => worker::EXIT_OK,
                    Ok(true) => worker::EXIT_FAILED,
                    Err(_) => worker::EXIT_PANICKED,
                };
                // Safety: _exit is always safe; skips atexit/stdio flushing,
                // which is what a forked worker wants.
                unsafe { libc::_exit(code) }
            }
            child_pid => {
                self.active.insert(child_pid, (slot, job, Instant::now()));
                self.stats.max_active_workers =
                    self.stats.max_active_workers.max(self.active.len());
            }
        }
    }

    /// Wait for one worker to finish and feed its observations to the
    /// controller.
    ///
    /// Polls the workers this controller forked, by pid and without blocking
    /// (never `waitpid(-1)`, which would reap an embedding harness's own
    /// children), and kills any worker past the wall-clock ceiling.
    fn reap_one_and_process(&mut self) {
        #[cfg(unix)]
        {
            let mut polls = 0_u32;
            let mut sleep = MIN_POLL_SLEEP;
            loop {
                if let Some((pid, status)) = self.poll_active() {
                    self.finish_worker(pid, status);
                    return;
                }
                if self.active.is_empty() {
                    return;
                }
                if polls < SPIN_POLLS {
                    polls += 1;
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(sleep);
                    sleep = (sleep * 2).min(MAX_POLL_SLEEP);
                }
            }
        }
    }

    /// One pass over the live workers: the first that exited, or that was
    /// killed for outliving the ceiling, with its `waitpid` status (`None` for
    /// a worker whose result is lost). `None` when every worker still runs.
    #[cfg(unix)]
    fn poll_active(&mut self) -> Option<(libc::pid_t, Option<libc::c_int>)> {
        let now = Instant::now();
        for (&pid, (_, job, started)) in &self.active {
            match worker::poll_worker(pid) {
                worker::WorkerState::Exited(status) => return Some((pid, Some(status))),
                worker::WorkerState::Gone => return Some((pid, None)),
                worker::WorkerState::Running
                    if now.duration_since(*started) > self.worker_timeout =>
                {
                    eprintln!(
                        "[explorer] worker {pid} exceeded the {:?} wall-clock ceiling; killed (recipe: {})",
                        self.worker_timeout,
                        crate::replay::format_timeline(&job.recipe)
                    );
                    return Some((pid, worker::kill_worker(pid)));
                }
                worker::WorkerState::Running => {}
            }
        }
        None
    }

    /// Consume one finished worker's result slot. A missing status (the
    /// worker was reaped elsewhere) counts as a crash with an empty journal.
    #[cfg(unix)]
    fn finish_worker(&mut self, pid: libc::pid_t, status: Option<libc::c_int>) {
        let Some((slot, job, _)) = self.active.remove(&pid) else {
            return;
        };
        let slots = self.slots.as_ref().expect("worker mode has slots");
        let (events, end_count) = if status.is_some() {
            slots.read_slot(slot)
        } else {
            (Vec::new(), 0)
        };
        // Novelty check doubles as merge into the sancov history.
        let new_edges = crate::sancov::has_new_pool_coverage(slot);
        self.free_slots.push(slot);

        let exit = status.map_or(WorkerExit::Crashed, worker::classify_exit);
        if exit == WorkerExit::Crashed {
            eprintln!(
                "[explorer] worker crashed (recipe: {})",
                crate::replay::format_timeline(&job.recipe)
            );
        }
        let run = RunResult {
            events: &events,
            failed: exit != WorkerExit::Ok,
            new_edges,
            end_count,
        };
        self.process_result(&job, &run, true);
    }

    // -----------------------------------------------------------------
    // Exploration policy
    // -----------------------------------------------------------------

    /// Consume one run's observations: account stats, register exemplars,
    /// and expand the run at most once if it was productive.
    ///
    /// A run is productive when it made a new assertion discovery, or, at
    /// low priority, when it reached new sancov edges and made none: that
    /// run gets one child, branched in the middle of its last replay
    /// segment, so coverage guides the sweep and not only its stop rule.
    fn process_result(&mut self, job: &ExploreJob, run: &RunResult<'_>, from_exploration: bool) {
        let RunResult {
            events,
            failed,
            new_edges,
            end_count,
        } = *run;
        self.stats.total_timelines += 1;
        self.stats.discoveries += events.len() as u64;
        if self.record {
            self.timelines.push(TimelineRecord {
                recipe: job.recipe.clone(),
                failed,
                discoveries: events.to_vec(),
            });
        }

        if failed && from_exploration {
            self.stats.bug_found += 1;
            if !job.recipe.is_empty() && self.bug_recipes.len() < MAX_BUG_RECIPES {
                self.bug_recipes.push(job.recipe.clone());
            }
        }

        let Some(anchor_event) = select_anchor(events) else {
            if new_edges {
                self.expand_on_edges(job, end_count);
            }
            // Otherwise a barren run: nothing globally new, the branch dies.
            return;
        };

        for event in events {
            self.register_state(&job.recipe, event);
        }

        // One expansion per productive run, anchored at the most useful
        // discovery. This keeps late plain-coverage noise from pulling the
        // immediate branch away from earlier monotonic progress.
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
        } else if self.states.len() < MAX_TRACKED_STATES {
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
        } else {
            return;
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
    /// One low-priority child for a run whose only novelty is new sancov
    /// edges. The edges carry no RNG coordinate, so the child branches in
    /// the middle of the run's last replay segment: late enough to keep a
    /// share of the path that found them, early enough to diverge.
    fn expand_on_edges(&mut self, job: &ExploreJob, end_count: u64) {
        if job.recipe.len() >= self.config.max_recipe_len
            || self.frontier.len() >= self.config.max_frontier
        {
            return;
        }
        let anchor = end_count / 2;
        let base_seed = job.recipe.last().map_or(self.root_seed, |&(_, seed)| seed);
        let seed = derive_seed(
            self.root_seed,
            base_seed,
            anchor,
            SANCOV_STATE_ID,
            self.stats.edge_expansions,
            0,
        );
        let mut recipe = job.recipe.clone();
        recipe.push((anchor, seed));
        self.frontier.push_back(ExploreJob { recipe });
        self.stats.edge_expansions += 1;
    }

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

/// Start a timeline with an empty journal and zeroed sancov counters.
fn begin_run() {
    journal::clear();
    crate::sancov::reset_bss_counters();
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

/// Choose the semantic discovery that should drive immediate expansion.
///
/// Progress toward an assertion goal outranks structured state novelty, which
/// outranks a one-shot coverage hit. Within the same class, the latest replay
/// coordinate preserves the deepest prefix.
/// The seed-derivation key of a sancov-only expansion: no assertion state
/// names it, so it takes a fixed id ("sancov" in ASCII).
const SANCOV_STATE_ID: u64 = 0x7361_6e63_6f76;

/// What one finished run reported to the controller.
#[derive(Clone, Copy)]
struct RunResult<'a> {
    /// Globally new discoveries of the assertion journal.
    events: &'a [DiscoveryEvent],
    /// The run failed (a simulation failure or a worker crash).
    failed: bool,
    /// The run reached sancov edges no earlier run reached.
    new_edges: bool,
    /// The RNG call count at the end of the run, in its last segment.
    end_count: u64,
}

fn select_anchor(events: &[DiscoveryEvent]) -> Option<DiscoveryEvent> {
    events
        .iter()
        .max_by_key(|event| event.retention_key())
        .copied()
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
        config.validate().expect("default config should be valid");
        assert_eq!(config.workers, 0);
        assert!(config.max_runs_per_seed > 0);
        assert!(config.branching_factor > 0);
        assert!(config.max_frontier > 0);
        assert!(config.max_recipe_len > 0);
    }

    #[test]
    fn zero_exploration_bounds_are_rejected() {
        let invalid = [
            ExplorationConfig {
                max_runs_per_seed: 0,
                ..ExplorationConfig::default()
            },
            ExplorationConfig {
                branching_factor: 0,
                ..ExplorationConfig::default()
            },
            ExplorationConfig {
                max_frontier: 0,
                ..ExplorationConfig::default()
            },
            ExplorationConfig {
                max_recipe_len: 0,
                ..ExplorationConfig::default()
            },
        ];

        for config in invalid {
            let error = config.validate().expect_err("zero bound must be rejected");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn zero_workers_is_valid() {
        let config = ExplorationConfig {
            workers: 0,
            max_runs_per_seed: 1,
            branching_factor: 1,
            max_frontier: 1,
            max_recipe_len: 1,
        };
        config
            .validate()
            .expect("in-process minimum should be valid");
    }

    /// A one-worker controller with a single job queued, for the process
    /// lifecycle tests below.
    #[cfg(unix)]
    fn one_worker_explorer() -> Explorer {
        let mut explorer = Explorer::new(ExplorationConfig {
            workers: 1,
            max_runs_per_seed: 1,
            ..ExplorationConfig::default()
        })
        .expect("explorer init");
        explorer.begin_seed(1);
        explorer
            .frontier
            .push_back(ExploreJob { recipe: Vec::new() });
        explorer
    }

    /// Whether `pid` still names a process (a zombie included).
    #[cfg(unix)]
    fn process_exists(pid: libc::pid_t) -> bool {
        // Safety: signal 0 only checks for existence and permission.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// A worker stuck outside the simulation is killed at the wall-clock
    /// ceiling and recorded as a crashed run, instead of blocking the
    /// controller forever (#262).
    #[cfg(unix)]
    #[test]
    fn a_hung_worker_is_killed_at_the_ceiling() {
        let mut explorer = one_worker_explorer();
        explorer.set_worker_timeout(Duration::from_millis(100));

        let started = Instant::now();
        explorer.explore(|_| {
            loop {
                std::thread::sleep(Duration::from_hours(1));
            }
        });

        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the ceiling must bound the wait, took {:?}",
            started.elapsed()
        );
        assert!(explorer.active.is_empty(), "the hung worker was reaped");
        assert_eq!(explorer.stats.total_timelines, 1);
        assert_eq!(explorer.stats.bug_found, 1, "a killed worker is a crash");
        drop(explorer);
        crate::cleanup_assertions();
    }

    /// Dropping the controller kills and reaps every live worker rather than
    /// orphaning it (#262).
    #[cfg(unix)]
    #[test]
    fn dropping_the_explorer_kills_live_workers() {
        let mut explorer = one_worker_explorer();
        let job = explorer.frontier.pop_front().expect("queued job");
        explorer.spawn_worker(job, &mut |_| loop {
            std::thread::sleep(Duration::from_hours(1));
        });
        let pid = *explorer.active.keys().next().expect("a live worker");
        assert!(process_exists(pid));

        drop(explorer);

        assert!(!process_exists(pid), "the worker was killed and reaped");
        crate::cleanup_assertions();
    }

    /// A timeline with new sancov edges and no new assertion state gets
    /// exactly one low-priority child, branched mid-segment; without new
    /// edges it is barren (#391).
    #[test]
    fn new_edges_alone_earn_one_expansion() {
        let mut explorer = Explorer::new(ExplorationConfig::default()).expect("explorer init");
        explorer.begin_seed(7);
        explorer.frontier.clear();
        let job = ExploreJob { recipe: Vec::new() };
        let barren = RunResult {
            events: &[],
            failed: false,
            new_edges: false,
            end_count: 1_000,
        };
        explorer.process_result(&job, &barren, true);
        assert!(explorer.frontier.is_empty(), "no novelty, no child");

        let edges = RunResult {
            new_edges: true,
            ..barren
        };
        explorer.process_result(&job, &edges, true);
        assert_eq!(
            explorer.frontier.len(),
            1,
            "one child, not a full branching"
        );
        let child = explorer.frontier.front().expect("one child");
        assert_eq!(child.recipe.len(), 1);
        assert_eq!(child.recipe[0].0, 500, "branched mid-segment");
        assert_eq!(explorer.stats.edge_expansions, 1);
        assert_eq!(explorer.stats.expansions, 0, "not an assertion expansion");
        drop(explorer);
        crate::cleanup_assertions();
    }

    #[test]
    fn semantic_progress_outranks_later_plain_coverage() {
        let events = [
            DiscoveryEvent {
                call_count: 12,
                kind: moonpool_assertions::DiscoveryKind::WatermarkImprovement,
                state_id: 1,
            },
            DiscoveryEvent {
                call_count: 900,
                kind: moonpool_assertions::DiscoveryKind::SometimesPass,
                state_id: 2,
            },
        ];

        assert_eq!(select_anchor(&events), Some(events[0]));
    }
}
