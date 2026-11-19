//! The contract between a model and the walk: [`Model`], its context
//! [`Cx`], and the log of what a run did.

use std::fmt;

use crate::{Mailbox, Rng};

/// Where a run is in its schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Faults are on: messages are dropped and duplicated, nodes crash and
    /// restart, every enabled action is drawn.
    Chaos,
    /// The faults stopped: nothing is dropped or duplicated, every node is
    /// asked to restart, and the model keeps moving.
    Quiet,
    /// The end of the tail: each round runs once every action that has a
    /// weight in this phase and empties the mailbox, until
    /// [`Model::converged`] holds.
    Settle,
}

/// A system of pure state machines that a [`Walk`](crate::Walk) drives.
///
/// The model owns the state machines under test, their disks, and the
/// oracles. The walk owns the schedule: it holds the messages in flight and
/// decides, from the seed, what happens next. There is no executor, no
/// network and no clock: time passes when the walk draws one of the model's
/// own [actions](Self::actions) (a tick, say).
///
/// Only [`new`](Self::new) and [`deliver`](Self::deliver) are required. A
/// model with nothing to crash leaves [`nodes`](Self::nodes) empty; a model
/// with no moves of its own leaves [`actions`](Self::actions) empty.
///
/// ## What the walk calls, and when
///
/// | Step | Phase | The walk calls |
/// |------|-------|----------------|
/// | deliver | all | [`deliver`](Self::deliver) with a random message in flight |
/// | drop, duplicate | chaos | nothing: the message disappears, or goes back in flight |
/// | crash | chaos | [`crash`](Self::crash) on a random node that is up |
/// | restart | chaos | [`restart`](Self::restart) on a random node that is down |
/// | a model move | all | [`act`](Self::act) with an action drawn by its [`weight`](Self::weight) in the phase |
/// | the faults stop | once | [`calm`](Self::calm) |
/// | each quiet step and settle round | quiet, settle | [`restart`](Self::restart) on every node that is down |
/// | after every delivery and every step | all | [`check`](Self::check) |
/// | before each settle round | settle | [`converged`](Self::converged) |
/// | the run converged | once | [`finish`](Self::finish) |
///
/// A failed `assert!` anywhere in the model fails the seed. The walk
/// reports the seed, the step and the last entries of the log.
pub trait Model: Sized {
    /// A message in flight between two parties. The walk clones it to
    /// duplicate it and to log it.
    type Msg: Clone + fmt::Debug;
    /// The name of a party that can crash: a node, a proxy, a disk. Use
    /// `()` when the model has one such party or none.
    type Node: Clone + PartialEq + fmt::Debug;
    /// One of the model's own moves: a client request, a timer tick, a
    /// step-down. Use `()` when the model has none.
    type Action: Copy + PartialEq + fmt::Debug + 'static;
    /// A fact the model records with [`Cx::record`] for a trace checker.
    /// Use `()` when the model records nothing.
    type Event: fmt::Debug;

    /// Build the model for one seed. Draw the deployment from
    /// [`Cx::rng`], and put the first messages in flight with [`Cx::send`].
    fn new(cx: &mut Cx<'_, Self>) -> Self;

    /// The model's moves. The alphabet is fixed. A move that is not
    /// possible in the current state does nothing in [`act`](Self::act).
    #[must_use]
    fn actions() -> &'static [Self::Action] {
        &[]
    }

    /// The weight of `action` in `phase`, against the weights of the walk's
    /// own steps (45 for a deliver step, 3 for a crash, 3 for a restart,
    /// unless set on the [`Walk`](crate::Walk)). One by default.
    ///
    /// Zero keeps a move out of a phase: no new client request in the quiet
    /// tail, no step-down while the run settles. In the settle phase the
    /// weight is a switch: each round makes once every move whose weight is
    /// not zero.
    #[must_use]
    fn weight(action: Self::Action, phase: Phase) -> u32 {
        let _ = (action, phase);
        1
    }

    /// Make one move.
    fn act(&mut self, action: Self::Action, cx: &mut Cx<'_, Self>) {
        let _ = (action, cx);
    }

    /// Hand `message` to the party it is for. A message for a party that is
    /// down is lost: drop it here.
    fn deliver(&mut self, message: Self::Msg, cx: &mut Cx<'_, Self>);

    /// Every party that can crash, in a stable order.
    #[must_use]
    fn nodes(&self) -> Vec<Self::Node> {
        Vec::new()
    }

    /// Whether `node` is up. The model holds the truth: a node may also
    /// crash by itself in the middle of [`deliver`](Self::deliver), between
    /// a write and its sync for example.
    #[must_use]
    fn is_up(&self, node: &Self::Node) -> bool {
        let _ = node;
        true
    }

    /// How likely the walk is to pick `node` for a crash or a restart,
    /// against the other nodes. One by default; zero keeps the walk's hands
    /// off a node.
    #[must_use]
    fn node_weight(&self, node: &Self::Node) -> u32 {
        let _ = node;
        1
    }

    /// Crash `node`: drop its volatile state, keep what its disk holds.
    fn crash(&mut self, node: &Self::Node, cx: &mut Cx<'_, Self>) {
        let _ = (node, cx);
    }

    /// Restart `node` from its disk. A model may refuse (a node that stays
    /// dead for the tail): the walk reads [`is_up`](Self::is_up) afterwards.
    fn restart(&mut self, node: &Self::Node, cx: &mut Cx<'_, Self>) {
        let _ = (node, cx);
    }

    /// The faults just stopped. Called once, before the walk asks every
    /// node that is down to restart. The place to draw the nodes that stay
    /// dead for the tail.
    fn calm(&mut self, cx: &mut Cx<'_, Self>) {
        let _ = cx;
    }

    /// The oracles: what must hold after every delivery and every step.
    /// Fail with `assert!`.
    fn check(&mut self) {}

    /// The liveness claim, judged at the end of the quiet tail: `Err` says
    /// what is still missing. The walk fails the seed, with that text, when
    /// this is still an error after the last settle round.
    ///
    /// # Errors
    ///
    /// What the run has not reached yet.
    fn converged(&self) -> Result<(), String> {
        Ok(())
    }

    /// The run converged. The place for the facts and the claims that hold
    /// only at the end of a run (`sometimes!` on what the whole run did).
    fn finish(&mut self) {}

    /// The messages of the `reachable!` sites that some seed of a campaign
    /// must reach. A site that no seed evaluates is otherwise invisible.
    /// Keep each message in a `const`, so that the site and this list share
    /// it.
    #[must_use]
    fn reachable_sites() -> &'static [&'static str] {
        &[]
    }

    /// The messages of the `sometimes!` sites that some seed of a campaign
    /// must pass, as [`reachable_sites`](Self::reachable_sites).
    #[must_use]
    fn sometimes_sites() -> &'static [&'static str] {
        &[]
    }

    /// The state to print when a seed fails.
    #[must_use]
    fn describe(&self) -> String {
        String::new()
    }
}

/// What a model can do while the walk calls it: draw, send, record.
pub struct Cx<'a, M: Model> {
    pub(crate) core: &'a mut Core<M>,
}

impl<M: Model> Cx<'_, M> {
    /// The seed of this run.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.core.seed
    }

    /// The step the walk is on, from zero.
    #[must_use]
    pub fn step(&self) -> u64 {
        self.core.step
    }

    /// The step within the current phase, from zero: for a move that runs
    /// only early in the quiet tail, or once at a given step of it.
    #[must_use]
    pub fn phase_step(&self) -> u64 {
        self.core.step - self.core.phase_start
    }

    /// Whether faults are on, off, or the run is settling.
    #[must_use]
    pub fn phase(&self) -> Phase {
        self.core.phase
    }

    /// The generator of this run. Every choice of the model comes from it,
    /// so that a seed replays.
    pub fn rng(&mut self) -> &mut Rng {
        &mut self.core.rng
    }

    /// Put `message` in flight. The walk delivers it later, in any order,
    /// or never.
    pub fn send(&mut self, message: M::Msg) {
        self.core.send(message);
    }

    /// Record a fact of the run, in order, for a trace checker. See
    /// [`Run::events`].
    pub fn record(&mut self, event: M::Event) {
        self.core.stats.events += 1;
        self.core.note(Entry::Event(event));
    }

    /// Say that `node` just crashed by itself, in the middle of a delivery
    /// for example: the log and the counts then show it. The model keeps the
    /// truth ([`Model::is_up`]); this only records the fact.
    pub fn crashed(&mut self, node: &M::Node) {
        self.core.stats.crashes += 1;
        self.core.note(Entry::Crash(node.clone()));
    }

    /// Lose every message in flight that `lost` admits: a partition, or the
    /// network of a staged tail that starts empty. Returns how many were
    /// lost.
    pub fn drop_where(&mut self, lost: impl FnMut(&M::Msg) -> bool) -> usize {
        self.core.drop_where(lost)
    }

    /// How many messages are in flight.
    #[must_use]
    pub fn inflight(&self) -> usize {
        self.core.mailbox.len()
    }
}

/// One thing a run did.
pub enum Entry<M: Model> {
    /// A message reached the model.
    Deliver(M::Msg),
    /// A message in flight was lost: dropped by the walk, or by the model
    /// with [`Cx::drop_where`].
    Drop(M::Msg),
    /// A message was delivered and also put back in flight.
    Duplicate(M::Msg),
    /// The mailbox was full and lost this message.
    Evict(M::Msg),
    /// A node crashed: the walk crashed it, or the model reported it with
    /// [`Cx::crashed`].
    Crash(M::Node),
    /// A node restarted.
    Restart(M::Node),
    /// The model made a move.
    Act(M::Action),
    /// The faults stopped.
    Calm,
    /// The model recorded a fact with [`Cx::record`].
    Event(M::Event),
}

impl<M: Model> fmt::Debug for Entry<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deliver(m) => write!(f, "deliver {m:?}"),
            Self::Drop(m) => write!(f, "drop {m:?}"),
            Self::Duplicate(m) => write!(f, "duplicate {m:?}"),
            Self::Evict(m) => write!(f, "evict {m:?}"),
            Self::Crash(n) => write!(f, "crash {n:?}"),
            Self::Restart(n) => write!(f, "restart {n:?}"),
            Self::Act(a) => write!(f, "act {a:?}"),
            Self::Calm => write!(f, "calm"),
            Self::Event(e) => write!(f, "event {e:?}"),
        }
    }
}

/// An [`Entry`] with the step and the phase it happened in.
pub struct Logged<M: Model> {
    /// The step, from zero.
    pub step: u64,
    /// The phase of the run at that step.
    pub phase: Phase,
    /// What happened.
    pub entry: Entry<M>,
}

impl<M: Model> fmt::Debug for Logged<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:>6} {:<6} {:?}",
            self.step,
            phase_name(self.phase),
            self.entry
        )
    }
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Chaos => "chaos",
        Phase::Quiet => "quiet",
        Phase::Settle => "settle",
    }
}

/// Counts of what one run, or a whole campaign, did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Steps of the walk.
    pub steps: u64,
    /// Messages handed to the model.
    pub delivered: u64,
    /// Messages dropped by the walk.
    pub dropped: u64,
    /// Messages duplicated by the walk.
    pub duplicated: u64,
    /// Messages lost to a full mailbox.
    pub evicted: u64,
    /// Crashes made by the walk, and the ones the model reported with
    /// [`Cx::crashed`].
    pub crashes: u64,
    /// Restarts after which the node was up.
    pub restarts: u64,
    /// Facts recorded by the model.
    pub events: u64,
    /// How many times each action of [`Model::actions`] ran, in the order
    /// of that list.
    pub actions: Vec<u64>,
}

impl Stats {
    /// Add the counts of `other` to these.
    pub fn absorb(&mut self, other: &Self) {
        self.steps += other.steps;
        self.delivered += other.delivered;
        self.dropped += other.dropped;
        self.duplicated += other.duplicated;
        self.evicted += other.evicted;
        self.crashes += other.crashes;
        self.restarts += other.restarts;
        self.events += other.events;
        if self.actions.len() < other.actions.len() {
            self.actions.resize(other.actions.len(), 0);
        }
        for (mine, theirs) in self.actions.iter_mut().zip(&other.actions) {
            *mine += theirs;
        }
    }
}

/// The part of a run that is not the model: what a [`Cx`] reaches.
pub(crate) struct Core<M: Model> {
    pub(crate) seed: u64,
    pub(crate) step: u64,
    /// The step at which the current phase started.
    pub(crate) phase_start: u64,
    pub(crate) phase: Phase,
    pub(crate) rng: Rng,
    pub(crate) mailbox: Mailbox<M::Msg>,
    pub(crate) log: Vec<Logged<M>>,
    pub(crate) stats: Stats,
}

impl<M: Model> Core<M> {
    pub(crate) fn new(seed: u64, mailbox: usize) -> Self {
        Self {
            seed,
            step: 0,
            phase_start: 0,
            phase: Phase::Chaos,
            rng: Rng::new(seed),
            mailbox: Mailbox::new(mailbox),
            log: Vec::new(),
            stats: Stats {
                actions: vec![0; M::actions().len()],
                ..Stats::default()
            },
        }
    }

    pub(crate) fn note(&mut self, entry: Entry<M>) {
        self.log.push(Logged {
            step: self.step,
            phase: self.phase,
            entry,
        });
    }

    pub(crate) fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.phase_start = self.step;
    }

    pub(crate) fn drop_where(&mut self, mut lost: impl FnMut(&M::Msg) -> bool) -> usize {
        let mut count = 0;
        while let Some(message) = self.mailbox.take_where(&mut lost) {
            self.stats.dropped += 1;
            self.note(Entry::Drop(message));
            count += 1;
        }
        count
    }

    pub(crate) fn send(&mut self, message: M::Msg) {
        if let Some(evicted) = self.mailbox.push(message, &mut self.rng) {
            self.stats.evicted += 1;
            self.note(Entry::Evict(evicted));
        }
    }
}

/// One finished run of one seed: the model as the run left it, and
/// everything the run did.
pub struct Run<M: Model> {
    /// The seed: the root seed of [`timeline`](Self::timeline).
    pub seed: u64,
    /// The timeline: the seed, and the branch points of an exploring
    /// campaign ([`Walk::explore`](crate::Walk::explore)).
    pub timeline: crate::Timeline,
    /// The chaos steps after which this run found something new: a first
    /// pass in the assertion table, or more covered edges. Empty unless the
    /// campaign explores.
    pub discoveries: Vec<u64>,
    /// The model at the end of the run.
    pub model: M,
    /// Everything the run did, in order.
    pub log: Vec<Logged<M>>,
    /// The counts of the run.
    pub stats: Stats,
}

impl<M: Model> Run<M> {
    /// The facts the model recorded with [`Cx::record`], in order: the
    /// input of a trace checker.
    pub fn events(&self) -> impl Iterator<Item = &M::Event> {
        self.log.iter().filter_map(|logged| match &logged.entry {
            Entry::Event(event) => Some(event),
            _ => None,
        })
    }
}
