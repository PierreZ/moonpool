//! The seeded schedule: [`Walk`] draws the steps of a run from its seed and
//! runs a campaign of seeds.

use std::cell::Cell;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};

use moonpool_assertions::{AssertKind, AssertionSlotSnapshot};

use crate::harness::{DEFAULT_MAILBOX, Harness};
use crate::model::Entry;
use crate::report::{Report, SeedFailure, Site, WalkError};
use crate::{Cx, Model, Phase, Rng, Run};

/// The environment prefix a [`Walk`] reads, unless set.
pub const DEFAULT_ENV_PREFIX: &str = "MOONPOOL_WALK";

/// Mixed into the seed for the stream that buggify draws from.
const BUGGIFY_SALT: u64 = 0xB066_1F1E_D5EE_D000;
/// Mixed into the seed for the stream that the swarm draws from.
const SWARM_SALT: u64 = 0x5A4A_3A2A_1A0A_F00D;

thread_local! {
    /// The state of the generator that buggify draws from. Buggify takes a
    /// plain function pointer, so the state lives here.
    static BUGGIFY_STREAM: Cell<u64> = const { Cell::new(0) };
}

fn buggify_draw() -> f64 {
    BUGGIFY_STREAM.with(|stream| {
        let mut rng = Rng::new(stream.get());
        let draw = rng.unit();
        stream.set(rng.next_u64());
        draw
    })
}

/// A seeded random walk over a [`Model`].
///
/// A run has three phases:
///
/// 1. **Chaos.** Each step is one draw, by weight: deliver some messages in
///    random order (each may be dropped or duplicated), crash a node,
///    restart a node, or one move of the model.
/// 2. **Quiet.** The faults stop. At each step every node that is down is
///    asked to restart, nothing is dropped or duplicated, and the walk keeps
///    delivering and moving.
/// 3. **Settle.** Rounds of "every action of this phase once, then empty
///    the mailbox", until [`Model::converged`] holds. A run that never
///    converges fails.
///
/// [`Model::check`] runs after every delivery and every step. A campaign
/// runs many seeds and stops at the first one that fails.
///
/// ## Buggify and assertions
///
/// For each seed the walk installs a seeded source into
/// [`moonpool_buggify`], so `buggify!` sites in the code under test fire.
/// The disruptive sites (`buggify_fault_with_prob!`, `buggify_named!`) fall
/// silent in the quiet tail; `buggify!` and `buggify_with_prob!` keep
/// firing. It installs an assertion table for [`moonpool_assertions`]: a
/// `sometimes!` or `reachable!` site that no seed passes fails the
/// campaign, and an always-style assertion that records a failure fails
/// the seed at the step that records it.
///
/// Both are thread-local. A walk that starts inside a running simulation
/// leaves buggify alone. A walk that finds an assertion table already
/// installed counts in it, but judges no coverage and declares no site:
/// the table's owner does.
///
/// A failed `assert!` of the model is caught for each seed, which needs
/// `panic = "unwind"` (the default of a test build).
///
/// ## Environment
///
/// A walk reads five variables, so that a long run or the replay of one
/// seed needs no code change. With the default prefix:
///
/// | Variable | Meaning |
/// |----------|---------|
/// | `MOONPOOL_WALK_SEED` | run this one seed, and skip the coverage check |
/// | `MOONPOOL_WALK_TAIL` | how many entries of the log a failure prints |
/// | `MOONPOOL_WALK_SEEDS` | how many seeds to run |
/// | `MOONPOOL_WALK_FROM` | the first seed |
/// | `MOONPOOL_WALK_STEPS` | the chaos steps of each seed |
///
/// A value that is not a number fails the campaign. Every walk with the
/// default prefix reads the same variables:
/// [`env_prefix`](Self::env_prefix) gives each model its own names.
pub struct Walk<M: Model> {
    seeds: u64,
    first_seed: u64,
    chaos_steps: u64,
    quiet_steps: u64,
    settle_rounds: u64,
    drain_steps: u64,
    mailbox: usize,
    deliver_weight: u32,
    quiet_deliver_weight: u32,
    deliveries: u32,
    quiet_deliveries: u32,
    crash_weight: u32,
    restart_weight: u32,
    max_down: Option<usize>,
    drop: f64,
    duplicate: f64,
    swarm: bool,
    buggify: Option<f64>,
    env_prefix: Option<String>,
    expected: Vec<(AssertKind, String)>,
    failure_tail: usize,
    check_each_delivery: bool,
    make: Option<Factory<M>>,
    model: PhantomData<fn() -> M>,
}

/// What builds the model of a seed in place of [`Model::new`].
type Factory<M> = Box<dyn Fn(&mut Cx<'_, M>) -> M>;

impl<M: Model> Default for Walk<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: Model> Walk<M> {
    /// A walk with the default schedule: 100 seeds from seed 1; 600 chaos
    /// steps, 400 quiet steps and at most 200 settle rounds for each; 10%
    /// of drops and of duplicates; a mailbox of 128 messages.
    #[must_use]
    pub fn new() -> Self {
        Self {
            seeds: 100,
            first_seed: 1,
            chaos_steps: 600,
            quiet_steps: 400,
            settle_rounds: 200,
            drain_steps: 4_000,
            mailbox: DEFAULT_MAILBOX,
            deliver_weight: 45,
            quiet_deliver_weight: 45,
            deliveries: 3,
            quiet_deliveries: 8,
            crash_weight: 3,
            restart_weight: 3,
            max_down: None,
            drop: 0.1,
            duplicate: 0.1,
            swarm: false,
            buggify: Some(0.5),
            env_prefix: Some(DEFAULT_ENV_PREFIX.to_owned()),
            expected: Vec::new(),
            failure_tail: 40,
            check_each_delivery: true,
            make: None,
            model: PhantomData,
        }
    }

    /// How many seeds a campaign runs.
    #[must_use]
    pub fn seeds(mut self, seeds: u64) -> Self {
        self.seeds = seeds;
        self
    }

    /// The first seed of a campaign. The others follow it.
    #[must_use]
    pub fn first_seed(mut self, seed: u64) -> Self {
        self.first_seed = seed;
        self
    }

    /// How many steps each seed runs with faults on.
    #[must_use]
    pub fn chaos_steps(mut self, steps: u64) -> Self {
        self.chaos_steps = steps;
        self
    }

    /// How many steps each seed runs after the faults stop.
    #[must_use]
    pub fn quiet_steps(mut self, steps: u64) -> Self {
        self.quiet_steps = steps;
        self
    }

    /// How many settle rounds a seed gets to converge before it fails.
    #[must_use]
    pub fn settle_rounds(mut self, rounds: u64) -> Self {
        self.settle_rounds = rounds;
        self
    }

    /// How many messages one settle round delivers at most.
    #[must_use]
    pub fn drain_steps(mut self, steps: u64) -> Self {
        self.drain_steps = steps;
        self
    }

    /// How many messages are in flight at most. One more evicts a random
    /// message, in every phase.
    #[must_use]
    pub fn mailbox(mut self, bound: usize) -> Self {
        self.mailbox = bound;
        self
    }

    /// The weight of a deliver step in the chaos phase and in the quiet
    /// phase, against the weights of the crash and restart steps and of the
    /// model's actions ([`Model::weight`]).
    #[must_use]
    pub fn deliver_weights(mut self, chaos: u32, quiet: u32) -> Self {
        self.deliver_weight = chaos;
        self.quiet_deliver_weight = quiet;
        self
    }

    /// How many messages one deliver step takes, in the chaos phase and in
    /// the quiet phase.
    #[must_use]
    pub fn deliveries_per_step(mut self, chaos: u32, quiet: u32) -> Self {
        self.deliveries = chaos;
        self.quiet_deliveries = quiet;
        self
    }

    /// The weights of the crash step and of the restart step. Zero switches
    /// a step off.
    #[must_use]
    pub fn crash_weights(mut self, crash: u32, restart: u32) -> Self {
        self.crash_weight = crash;
        self.restart_weight = restart;
        self
    }

    /// The most nodes the walk keeps down at once. A node that crashes by
    /// itself does not ask.
    #[must_use]
    pub fn max_down(mut self, nodes: usize) -> Self {
        self.max_down = Some(nodes);
        self
    }

    /// The probability that a message taken in the chaos phase is lost.
    #[must_use]
    pub fn drop_probability(mut self, probability: f64) -> Self {
        self.drop = probability;
        self
    }

    /// The probability that a message delivered in the chaos phase also
    /// goes back in flight.
    #[must_use]
    pub fn duplicate_probability(mut self, probability: f64) -> Self {
        self.duplicate = probability;
        self
    }

    /// Build the model of each seed with `make`, not with [`Model::new`]:
    /// for a model that takes a configuration, or a mutant switch.
    #[must_use]
    pub fn model(mut self, make: impl Fn(&mut Cx<'_, M>) -> M + 'static) -> Self {
        self.make = Some(Box::new(make));
        self
    }

    /// How many of the last entries of the log a [`SeedFailure`] keeps (40
    /// by default). `usize::MAX` keeps the whole log.
    #[must_use]
    pub fn failure_tail(mut self, entries: usize) -> Self {
        self.failure_tail = entries;
        self
    }

    /// Whether [`Model::check`] runs after every delivery (the default), or
    /// only at the end of each step: a step delivers several messages, and
    /// a settle round delivers thousands. Switch it off for an oracle that
    /// is too slow for that.
    #[must_use]
    pub fn check_each_delivery(mut self, on: bool) -> Self {
        self.check_each_delivery = on;
        self
    }

    /// Swarm testing: each seed switches off a random subset of the walk's
    /// fault families (drops, duplicates, crashes) and of the model's
    /// actions for its chaos phase, and keeps at least one action. A seed
    /// that never crashes goes deeper in the protocol; a seed that never
    /// proposes makes elections collide. The quiet tail runs every action.
    #[must_use]
    pub fn swarm(mut self, on: bool) -> Self {
        self.swarm = on;
        self
    }

    /// The probability that a `buggify!` location is active for a seed
    /// (0.5 by default, as in `moonpool-sim`). `None` leaves buggify alone.
    #[must_use]
    pub fn buggify(mut self, activation: Option<f64>) -> Self {
        self.buggify = activation;
        self
    }

    /// The prefix of the environment variables this walk reads
    /// (`PROXY_MODEL` gives `PROXY_MODEL_SEED`, `PROXY_MODEL_SEEDS`,
    /// `PROXY_MODEL_FROM`, `PROXY_MODEL_STEPS` and `PROXY_MODEL_TAIL`).
    /// `None` reads nothing.
    #[must_use]
    pub fn env_prefix(mut self, prefix: Option<&str>) -> Self {
        self.env_prefix = prefix.map(str::to_owned);
        self
    }

    /// Declare a `sometimes!` site by its message. A site that no seed
    /// reaches is otherwise invisible; a declared one fails the campaign.
    /// [`Model::sometimes_sites`] declares the sites of a model for every
    /// walk.
    #[must_use]
    pub fn expect_sometimes(mut self, message: &str) -> Self {
        self.expected
            .push((AssertKind::Sometimes, message.to_owned()));
        self
    }

    /// Declare a `reachable!` site by its message, as
    /// [`expect_sometimes`](Self::expect_sometimes) does.
    #[must_use]
    pub fn expect_reachable(mut self, message: &str) -> Self {
        self.expected
            .push((AssertKind::Reachable, message.to_owned()));
        self
    }

    /// Run the campaign: what a test calls.
    ///
    /// # Panics
    ///
    /// When a seed fails or the campaign has a coverage gap, with the
    /// [`WalkError`] as the message.
    #[track_caller]
    pub fn run(&self) {
        let _ = self.report();
    }

    /// Run the campaign and return its counts.
    ///
    /// # Panics
    ///
    /// As [`run`](Self::run).
    #[must_use]
    #[track_caller]
    pub fn report(&self) -> Report {
        match self.try_run() {
            Ok(report) => report,
            Err(error) => panic!("{error}"),
        }
    }

    /// Run the campaign and return its failure instead of a panic: for a
    /// test that proves a mutant is caught.
    ///
    /// # Errors
    ///
    /// The first seed that fails, the coverage gaps of a campaign whose
    /// seeds all passed, or why the campaign could not start.
    pub fn try_run(&self) -> Result<Report, WalkError> {
        self.run_each(|_| {})
    }

    /// Run the campaign and hand each finished [`Run`] to `visit`: the
    /// place to give [`Run::events`] to a trace checker.
    ///
    /// # Errors
    ///
    /// As [`try_run`](Self::try_run).
    pub fn run_each(&self, mut visit: impl FnMut(&Run<M>)) -> Result<Report, WalkError> {
        let plan = self.plan().map_err(WalkError::Setup)?;
        if plan.count == 0 {
            return Err(WalkError::Setup("the campaign has no seed to run".into()));
        }
        let installed = Installed::new(self);
        let mut judge = Judge::new();
        let mut report = Report {
            actions: M::actions().iter().map(|a| format!("{a:?}")).collect(),
            ..Report::default()
        };
        for offset in 0..plan.count {
            // Seeds wrap at the end of `u64`: a campaign is never cut short.
            let seed = plan.first.wrapping_add(offset);
            let run = self
                .one_seed(&plan, &installed, &mut judge, seed)
                .map_err(WalkError::Seed)?;
            report.seeds += 1;
            report.stats.absorb(&run.stats);
            visit(&run);
        }
        if !installed.table {
            // The table is not this walk's: its owner judges the coverage.
            return Ok(report);
        }
        let (sites, gaps) = coverage();
        report.sites = sites;
        if plan.pinned || gaps.is_empty() {
            Ok(report)
        } else {
            Err(WalkError::Coverage(gaps))
        }
    }

    /// Run one seed and return everything it did. No coverage is judged.
    ///
    /// # Errors
    ///
    /// The failure of the seed. [`failure_tail`](Self::failure_tail) sets
    /// how much of its log the failure keeps.
    pub fn replay(&self, seed: u64) -> Result<Run<M>, Box<SeedFailure>> {
        let plan = match self.plan() {
            Ok(plan) => Plan {
                first: seed,
                count: 1,
                pinned: true,
                ..plan
            },
            Err(why) => {
                return Err(Box::new(SeedFailure {
                    seed,
                    step: 0,
                    message: why,
                    tail: Vec::new(),
                    state: String::new(),
                    swarm: String::new(),
                    replay: None,
                }));
            }
        };
        let installed = Installed::new(self);
        let mut judge = Judge::new();
        self.one_seed(&plan, &installed, &mut judge, seed)
    }

    /// Run `seed` twice and compare the two logs: a model that reads a
    /// clock, a `HashMap` order or a generator of its own is caught here.
    ///
    /// # Errors
    ///
    /// The first entry where the two runs differ.
    pub fn check_determinism(&self, seed: u64) -> Result<(), String> {
        let lines = |outcome: Result<Run<M>, Box<SeedFailure>>| match outcome {
            Ok(run) => run.log.iter().map(|l| format!("{l:?}")).collect::<Vec<_>>(),
            Err(failure) => {
                let mut lines = failure.tail;
                lines.push(format!(
                    "failed at step {}: {}",
                    failure.step, failure.message
                ));
                lines
            }
        };
        let first = lines(self.replay(seed));
        let second = lines(self.replay(seed));
        if first == second {
            return Ok(());
        }
        let at = first
            .iter()
            .zip(&second)
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| first.len().min(second.len()));
        let show = |lines: &[String]| lines.get(at).cloned().unwrap_or_else(|| "the end".into());
        Err(format!(
            "seed {seed} is not deterministic: entry {at} is `{}` in one run and `{}` in the other",
            show(&first),
            show(&second)
        ))
    }

    /// Run the tail of a walk on a scripted [`Harness`]: stop the faults,
    /// run the quiet steps, then settle until [`Model::converged`] holds,
    /// and call [`Model::finish`]. A scripted prefix followed by this is a
    /// regression test with the liveness claim of the walk.
    ///
    /// The harness draws from its own seed. Buggify and the assertion table
    /// are not installed: this is a script, not a campaign.
    ///
    /// # Errors
    ///
    /// Why the model did not converge.
    pub fn finish(&self, harness: &mut Harness<M>) -> Result<(), String> {
        self.tail(harness, &mut Judge::off())
    }

    /// The schedule after the environment had its say.
    fn plan(&self) -> Result<Plan, String> {
        self.plan_from(|name| std::env::var(name).ok())
    }

    /// The schedule, with `lookup` as the environment.
    fn plan_from(&self, lookup: impl Fn(&str) -> Option<String>) -> Result<Plan, String> {
        let mut plan = Plan {
            first: self.first_seed,
            count: self.seeds,
            chaos_steps: self.chaos_steps,
            tail: self.failure_tail,
            pinned: false,
            replay: None,
        };
        let Some(prefix) = &self.env_prefix else {
            return Ok(plan);
        };
        let read = |name: &str| -> Result<Option<u64>, String> {
            let name = format!("{prefix}_{name}");
            match lookup(&name) {
                None => Ok(None),
                Some(text) => match text.parse() {
                    Ok(value) => Ok(Some(value)),
                    Err(_) => Err(format!("{name}={text} is not a number")),
                },
            }
        };
        let mut replay = format!("{prefix}_SEED=");
        if let Some(count) = read("SEEDS")? {
            plan.count = count;
        }
        if let Some(first) = read("FROM")? {
            plan.first = first;
        }
        if let Some(steps) = read("STEPS")? {
            plan.chaos_steps = steps;
            replay = format!("{prefix}_STEPS={steps} {replay}");
        }
        if let Some(tail) = read("TAIL")? {
            plan.tail = usize::try_from(tail).unwrap_or(usize::MAX);
        }
        if let Some(seed) = read("SEED")? {
            plan.first = seed;
            plan.count = 1;
            plan.pinned = true;
        }
        plan.replay = Some(replay);
        Ok(plan)
    }

    /// Run one seed. A panic of the model is the failure of the seed.
    fn one_seed(
        &self,
        plan: &Plan,
        installed: &Installed,
        judge: &mut Judge,
        seed: u64,
    ) -> Result<Run<M>, Box<SeedFailure>> {
        judge.every_step = false;
        judge.tripped = false;
        let first = self.attempt(plan, installed, judge, seed);
        if first.is_ok() || !judge.tripped {
            return first;
        }
        // The table holds a violation, found at the end of the run. The seed
        // replays: run it again and read the table after every step, to
        // stop at the step that records it.
        judge.every_step = true;
        judge.tripped = false;
        match self.attempt(plan, installed, judge, seed) {
            Err(located) if judge.tripped => Err(located),
            // The second run did not record it: the model does not replay.
            _ => first,
        }
    }

    /// One run of one seed.
    fn attempt(
        &self,
        plan: &Plan,
        installed: &Installed,
        judge: &mut Judge,
        seed: u64,
    ) -> Result<Run<M>, Box<SeedFailure>> {
        installed.start_seed(seed);
        let swarm = Swarm::draw(self, seed);
        let mut slot: Option<Harness<M>> = None;
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let harness = slot.insert(match &self.make {
                Some(make) => Harness::build(seed, self.mailbox, make),
                None => Harness::with_mailbox(seed, self.mailbox),
            });
            harness.model.check();
            judge.step()?;
            self.chaos(harness, plan, &swarm, judge)?;
            if installed.buggify.is_some() {
                moonpool_buggify::buggify_enter_recovery();
            }
            self.tail(harness, judge)?;
            judge.read()
        }));
        let message = match outcome {
            Ok(Ok(())) => None,
            Ok(Err(message)) => Some(message),
            Err(payload) => Some(panic_message(payload.as_ref())),
        };
        let (Some(message), harness) = (message, &slot) else {
            // The closure returned, so it filled the slot.
            return slot.map(Harness::into_run).ok_or_else(|| {
                Box::new(failure_of::<M>(
                    plan,
                    seed,
                    0,
                    "the run left no model".into(),
                    &[],
                ))
            });
        };
        let mut failure = match harness {
            Some(harness) => failure_of(plan, seed, harness.core.step, message, &harness.core.log),
            None => failure_of::<M>(plan, seed, 0, message, &[]),
        };
        failure.swarm = swarm.describe::<M>();
        if let Some(harness) = harness {
            // The model is in the state the panic left it in: its own
            // description may panic too.
            failure.state = catch_unwind(AssertUnwindSafe(|| harness.model.describe()))
                .unwrap_or_else(|_| "the model could not describe itself".into());
        }
        Err(Box::new(failure))
    }

    /// The end of a step: the oracles, the assertion table, then the count.
    fn close(harness: &mut Harness<M>, judge: &mut Judge) -> Result<(), String> {
        harness.model.check();
        judge.step()?;
        harness.count_step();
        Ok(())
    }

    /// The weights of one phase: deliver, crash, restart, then each action.
    fn weights(&self, phase: Phase, swarm: Option<&Swarm>) -> Vec<u32> {
        let chaos = phase == Phase::Chaos;
        let faults = chaos && swarm.is_none_or(|swarm| swarm.crash);
        let deliver = if chaos {
            self.deliver_weight
        } else {
            self.quiet_deliver_weight
        };
        let mut weights = vec![
            deliver,
            if faults { self.crash_weight } else { 0 },
            if faults { self.restart_weight } else { 0 },
        ];
        weights.extend(M::actions().iter().enumerate().map(|(index, action)| {
            let off = swarm.is_some_and(|swarm| swarm.actions.get(index) == Some(&false));
            if off { 0 } else { M::weight(*action, phase) }
        }));
        weights
    }

    /// The chaos phase of one run.
    fn chaos(
        &self,
        harness: &mut Harness<M>,
        plan: &Plan,
        swarm: &Swarm,
        judge: &mut Judge,
    ) -> Result<(), String> {
        let actions = M::actions();
        let weights = self.weights(Phase::Chaos, Some(swarm));
        for _ in 0..plan.chaos_steps {
            match harness.core.rng.weighted(&weights) {
                Some(0) => {
                    for _ in 0..self.deliveries {
                        self.deliver_one(harness, swarm);
                    }
                }
                Some(1) => self.crash_one(harness),
                Some(2) => Self::restart_one(harness),
                Some(index) => {
                    if let Some(action) = actions.get(index - 3) {
                        harness.act_unchecked(*action);
                    }
                }
                None => {}
            }
            Self::close(harness, judge)?;
        }
        Ok(())
    }

    /// The quiet phase, the settle rounds and the end of one run.
    fn tail(&self, harness: &mut Harness<M>, judge: &mut Judge) -> Result<(), String> {
        let actions = M::actions();
        harness.calm_unchecked();
        Self::close(harness, judge)?;
        let weights = self.weights(Phase::Quiet, None);
        for _ in 0..self.quiet_steps {
            harness.revive();
            match harness.core.rng.weighted(&weights) {
                Some(0) => {
                    for _ in 0..self.quiet_deliveries {
                        self.deliver_quiet(harness);
                    }
                }
                Some(index) if index >= 3 => {
                    if let Some(action) = actions.get(index - 3) {
                        harness.act_unchecked(*action);
                    }
                }
                _ => {}
            }
            Self::close(harness, judge)?;
        }

        harness.core.enter(Phase::Settle);
        let mut round = 0;
        while let Err(missing) = harness.model.converged() {
            if round == self.settle_rounds {
                return Err(format!(
                    "the model did not converge after {} quiet steps and {} settle rounds: {missing}",
                    self.quiet_steps, self.settle_rounds
                ));
            }
            round += 1;
            harness.revive();
            for action in actions {
                if M::weight(*action, Phase::Settle) > 0 {
                    harness.act_unchecked(*action);
                }
            }
            let mut delivered = 0;
            while delivered < self.drain_steps && self.deliver_quiet(harness) {
                delivered += 1;
            }
            Self::close(harness, judge)?;
        }
        harness.model.finish();
        Ok(())
    }

    /// Take one random message: lose it, or deliver it, and maybe put a
    /// copy back in flight.
    fn deliver_one(&self, harness: &mut Harness<M>, swarm: &Swarm) {
        let core = &mut harness.core;
        let Some(message) = core.mailbox.take(&mut core.rng) else {
            return;
        };
        if swarm.drop && core.rng.happens(self.drop) {
            core.stats.dropped += 1;
            core.note(Entry::Drop(message));
            return;
        }
        if swarm.duplicate && core.rng.happens(self.duplicate) {
            core.stats.duplicated += 1;
            core.note(Entry::Duplicate(message.clone()));
            core.send(message.clone());
        }
        harness.deliver_unchecked(message);
        if self.check_each_delivery {
            harness.model.check();
        }
    }

    /// Deliver one random message with no fault. `false` when nothing is in
    /// flight.
    fn deliver_quiet(&self, harness: &mut Harness<M>) -> bool {
        let core = &mut harness.core;
        let Some(message) = core.mailbox.take(&mut core.rng) else {
            return false;
        };
        harness.deliver_unchecked(message);
        if self.check_each_delivery {
            harness.model.check();
        }
        true
    }

    fn crash_one(&self, harness: &mut Harness<M>) {
        let nodes = harness.model.nodes();
        let down = nodes.iter().filter(|n| !harness.model.is_up(n)).count();
        if self.max_down.is_some_and(|max| down >= max) {
            return;
        }
        if let Some(node) = Self::pick_node(harness, &nodes, true) {
            harness.crash_unchecked(node);
        }
    }

    fn restart_one(harness: &mut Harness<M>) {
        let nodes = harness.model.nodes();
        if let Some(node) = Self::pick_node(harness, &nodes, false) {
            harness.restart_unchecked(node);
        }
    }

    /// One node that is up (or down), drawn by [`Model::node_weight`].
    fn pick_node<'n>(
        harness: &mut Harness<M>,
        nodes: &'n [M::Node],
        up: bool,
    ) -> Option<&'n M::Node> {
        let weights: Vec<u32> = nodes
            .iter()
            .map(|node| {
                if harness.model.is_up(node) == up {
                    harness.model.node_weight(node)
                } else {
                    0
                }
            })
            .collect();
        let index = harness.core.rng.weighted(&weights)?;
        nodes.get(index)
    }
}

/// The schedule of a campaign, after the environment.
struct Plan {
    first: u64,
    count: u64,
    chaos_steps: u64,
    /// How many entries of the log a failure keeps.
    tail: usize,
    /// One seed was asked for: coverage is not judged.
    pinned: bool,
    /// The variables that run one seed again, up to the `=` of the seed.
    replay: Option<String>,
}

fn failure_of<M: Model>(
    plan: &Plan,
    seed: u64,
    step: u64,
    message: String,
    log: &[crate::Logged<M>],
) -> SeedFailure {
    let from = log.len().saturating_sub(plan.tail);
    SeedFailure {
        seed,
        step,
        message,
        tail: log[from..].iter().map(|l| format!("{l:?}")).collect(),
        state: String::new(),
        swarm: String::new(),
        replay: plan
            .replay
            .as_ref()
            .map(|variables| format!("{variables}{seed}")),
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "the model panicked with a payload that is not a string".to_owned()
    }
}

/// What one seed keeps on in its chaos phase.
struct Swarm {
    drop: bool,
    duplicate: bool,
    crash: bool,
    actions: Vec<bool>,
}

impl Swarm {
    fn draw<M: Model>(walk: &Walk<M>, seed: u64) -> Self {
        let count = M::actions().len();
        if !walk.swarm {
            return Self {
                drop: true,
                duplicate: true,
                crash: true,
                actions: vec![true; count],
            };
        }
        let mut rng = Rng::new(seed ^ SWARM_SALT);
        let drop = rng.chance(1, 2);
        let duplicate = rng.chance(1, 2);
        let crash = rng.chance(1, 2);
        let mut actions: Vec<bool> = (0..count).map(|_| rng.chance(1, 2)).collect();
        // Keep one action that the chaos phase can draw.
        let drawable: Vec<usize> = M::actions()
            .iter()
            .enumerate()
            .filter(|(_, action)| M::weight(**action, Phase::Chaos) > 0)
            .map(|(index, _)| index)
            .collect();
        if !drawable.iter().any(|index| actions[*index])
            && let Some(index) = rng.pick(&drawable)
        {
            actions[*index] = true;
        }
        Self {
            drop,
            duplicate,
            crash,
            actions,
        }
    }

    /// What is off, or nothing when everything is on.
    fn describe<M: Model>(&self) -> String {
        let mut off = Vec::new();
        for (on, name) in [
            (self.drop, "drops"),
            (self.duplicate, "duplicates"),
            (self.crash, "crashes"),
        ] {
            if !on {
                off.push(name.to_owned());
            }
        }
        for (action, on) in M::actions().iter().zip(&self.actions) {
            if !on {
                off.push(format!("{action:?}"));
            }
        }
        if off.is_empty() {
            String::new()
        } else {
            format!("off for this seed's chaos phase: {}", off.join(", "))
        }
    }
}

/// What a walk installs into the thread for its campaign, and removes at
/// the end.
struct Installed {
    /// The assertion table is this walk's: it was absent before.
    table: bool,
    /// Buggify is this walk's: no simulation was running before.
    buggify: Option<f64>,
}

impl Installed {
    fn new<M: Model>(walk: &Walk<M>) -> Self {
        let table = moonpool_assertions::assertion_table_ptr().is_null();
        if table {
            // Sites are declared only in a table of this walk: a declaration
            // cannot be taken out of a table that outlives the campaign.
            moonpool_assertions::init();
            for (kind, message) in &walk.expected {
                moonpool_assertions::declare_site(*kind, true, false, message);
            }
            for message in M::reachable_sites() {
                moonpool_assertions::declare_site(AssertKind::Reachable, true, false, message);
            }
            for message in M::sometimes_sites() {
                moonpool_assertions::declare_site(AssertKind::Sometimes, true, false, message);
            }
        }
        let buggify = if moonpool_buggify::is_simulated() {
            None
        } else {
            walk.buggify
        };
        if buggify.is_some() {
            moonpool_buggify::set_random_source(buggify_draw);
        }
        Self { table, buggify }
    }

    fn start_seed(&self, seed: u64) {
        if let Some(activation) = self.buggify {
            BUGGIFY_STREAM.with(|stream| stream.set(seed ^ BUGGIFY_SALT));
            moonpool_buggify::buggify_init(activation);
        }
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        if self.buggify.is_some() {
            moonpool_buggify::buggify_reset();
            moonpool_buggify::clear_random_source();
        }
        if self.table {
            moonpool_assertions::clear();
        }
    }
}

/// Reads the violations of the assertion table, to fail the seed that
/// records one.
///
/// A read scans the whole table, so a run reads it once, at its end. When
/// that read finds a violation the walk runs the seed again with a read
/// after every step, to stop at the step that records it.
struct Judge {
    /// The violations already seen, or `None` when no table is read (a
    /// scripted tail).
    seen: Option<u64>,
    /// Read after every step, not only at the end of the run.
    every_step: bool,
    /// A read found a violation.
    tripped: bool,
}

impl Judge {
    /// A judge that starts from the count the table holds now: a table that
    /// was there before the walk may hold failures of someone else.
    fn new() -> Self {
        Self {
            seen: Some(violations().0),
            every_step: false,
            tripped: false,
        }
    }

    /// A judge that reads nothing.
    fn off() -> Self {
        Self {
            seen: None,
            every_step: false,
            tripped: false,
        }
    }

    /// The end of a step: a read only when every step is read.
    fn step(&mut self) -> Result<(), String> {
        if self.every_step { self.read() } else { Ok(()) }
    }

    /// Fail when an always-style assertion recorded a failure since the
    /// last read.
    fn read(&mut self) -> Result<(), String> {
        let Some(seen) = &mut self.seen else {
            return Ok(());
        };
        let (now, sites) = violations();
        if now <= *seen {
            return Ok(());
        }
        *seen = now;
        self.tripped = true;
        Err(sites.join("; "))
    }
}

/// How many violations the assertion table holds (the failures of the
/// always-style sites and the passes of the unreachable ones), and the
/// sites that hold one.
fn violations() -> (u64, Vec<String>) {
    let mut count = 0;
    let mut sites = Vec::new();
    for slot in moonpool_assertions::assertion_read_all() {
        let (bad, what) = match AssertKind::from_u8(slot.kind) {
            Some(
                AssertKind::Always | AssertKind::AlwaysOrUnreachable | AssertKind::NumericAlways,
            ) => (slot.fail_count, "an always assertion failed"),
            Some(AssertKind::Unreachable) => (slot.pass_count, "an unreachable site was reached"),
            _ => continue,
        };
        if bad > 0 {
            count += bad;
            sites.push(format!("{what}: {}", slot.msg));
        }
    }
    (count, sites)
}

/// The sometimes-style sites of the campaign, and the ones that never
/// passed.
fn coverage() -> (Vec<Site>, Vec<String>) {
    let mut sites = Vec::new();
    let mut gaps = Vec::new();
    for slot in moonpool_assertions::assertion_read_all() {
        if slot.conflicted {
            gaps.push(format!(
                "'{}' is used by two sites of different kinds, or two messages collide",
                slot.msg
            ));
        }
        if let Some(reached) = reached(&slot) {
            if !reached {
                gaps.push(format!("'{}' was never reached", slot.msg));
            }
            sites.push(Site {
                hits: slot.pass_count,
                message: slot.msg,
            });
        }
    }
    let dropped = moonpool_assertions::assertion_dropped_allocations();
    if dropped > 0 {
        gaps.push(format!(
            "the assertion table is full: {dropped} evaluations were not counted"
        ));
    }
    (sites, gaps)
}

/// Whether a sometimes-style site passed; `None` for another kind of site.
fn reached(slot: &AssertionSlotSnapshot) -> Option<bool> {
    match AssertKind::from_u8(slot.kind)? {
        AssertKind::Sometimes | AssertKind::Reachable | AssertKind::NumericSometimes => {
            Some(slot.pass_count > 0)
        }
        AssertKind::BooleanSometimesAll => Some(slot.frontier >= slot.frontier_target),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nothing;

    impl Model for Nothing {
        type Msg = ();
        type Node = ();
        type Action = ();
        type Event = ();
        fn new(_cx: &mut Cx<'_, Self>) -> Self {
            Self
        }
        fn deliver(&mut self, (): (), _cx: &mut Cx<'_, Self>) {}
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn the_environment_overrides_the_schedule() {
        let walk = Walk::<Nothing>::new()
            .seeds(10)
            .first_seed(5)
            .chaos_steps(20);
        let plan = walk.plan_from(env(&[])).expect("no variable");
        assert_eq!(
            (plan.first, plan.count, plan.chaos_steps, plan.tail),
            (5, 10, 20, 40)
        );
        assert!(!plan.pinned);
        assert_eq!(plan.replay.as_deref(), Some("MOONPOOL_WALK_SEED="));

        let plan = walk
            .plan_from(env(&[
                ("MOONPOOL_WALK_SEEDS", "5000"),
                ("MOONPOOL_WALK_FROM", "100"),
                ("MOONPOOL_WALK_STEPS", "7"),
                ("MOONPOOL_WALK_TAIL", "900"),
            ]))
            .expect("four numbers");
        assert_eq!(
            (plan.first, plan.count, plan.chaos_steps, plan.tail),
            (100, 5000, 7, 900)
        );
        assert_eq!(
            plan.replay.as_deref(),
            Some("MOONPOOL_WALK_STEPS=7 MOONPOOL_WALK_SEED="),
            "the hint keeps the steps that the failure ran with"
        );

        let plan = walk
            .plan_from(env(&[
                ("MOONPOOL_WALK_SEED", "42"),
                ("MOONPOOL_WALK_SEEDS", "9"),
            ]))
            .expect("two numbers");
        assert_eq!((plan.first, plan.count), (42, 1));
        assert!(plan.pinned, "one seed: coverage is not judged");
    }

    #[test]
    fn a_variable_that_is_not_a_number_stops_the_campaign() {
        let walk = Walk::<Nothing>::new();
        let error = walk
            .plan_from(env(&[("MOONPOOL_WALK_SEED", "abc")]))
            .err()
            .expect("not a number");
        assert_eq!(error, "MOONPOOL_WALK_SEED=abc is not a number");
    }

    #[test]
    fn a_prefix_names_the_variables_and_none_reads_nothing() {
        let pairs = [("PROXY_MODEL_SEED", "3"), ("MOONPOOL_WALK_SEED", "4")];
        let named = Walk::<Nothing>::new().env_prefix(Some("PROXY_MODEL"));
        let plan = named.plan_from(env(&pairs)).expect("a number");
        assert_eq!(plan.first, 3);
        assert_eq!(plan.replay.as_deref(), Some("PROXY_MODEL_SEED="));

        let deaf = Walk::<Nothing>::default().env_prefix(None);
        let plan = deaf.plan_from(env(&pairs)).expect("nothing is read");
        assert_eq!((plan.first, plan.count, plan.pinned), (1, 100, false));
        assert_eq!(plan.replay, None);
    }

    #[test]
    fn a_campaign_with_no_seed_does_not_pass() {
        let error = Walk::<Nothing>::new().seeds(0).env_prefix(None).try_run();
        assert!(matches!(error, Err(WalkError::Setup(_))), "{error:?}");
    }

    #[test]
    fn a_campaign_runs_the_last_seed_and_wraps() {
        let mut seeds = Vec::new();
        let report = Walk::<Nothing>::new()
            .first_seed(u64::MAX)
            .seeds(2)
            .env_prefix(None)
            .run_each(|run| seeds.push(run.seed))
            .expect("two seeds pass");
        assert_eq!(report.seeds, 2);
        assert_eq!(seeds, [u64::MAX, 0]);
    }

    #[test]
    fn a_model_with_nothing_to_do_passes() {
        let report = Walk::<Nothing>::new().seeds(2).env_prefix(None).report();
        assert_eq!(report.seeds, 2);
        assert_eq!(report.stats.delivered, 0);
        assert!(report.actions.is_empty() && report.sites.is_empty());
    }

    #[test]
    fn the_walk_removes_what_it_installed() {
        Walk::<Nothing>::new().seeds(1).env_prefix(None).run();
        assert!(moonpool_assertions::assertion_table_ptr().is_null());
        assert!(!moonpool_buggify::is_simulated());
    }
}
