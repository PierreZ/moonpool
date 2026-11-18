//! One model and its messages in flight, moved one step at a time.

use crate::model::{Core, Entry, Logged, Stats};
use crate::{Cx, Mailbox, Model, Phase, Rng, Run};

/// The mailbox bound of a [`Harness`] and of a [`Walk`](crate::Walk), unless
/// set.
pub const DEFAULT_MAILBOX: usize = 128;

/// One model and its messages in flight, moved one step at a time.
///
/// A [`Walk`](crate::Walk) draws its steps from a seed. A scripted test
/// writes them by hand with the same engine: this is how a schedule that a
/// seed found once becomes a named regression test.
///
/// Every method that moves the model runs [`Model::check`] afterwards.
///
/// ```rust
/// use moonpool_walk::{Cx, Harness, Model};
///
/// /// A register that keeps the highest value it saw.
/// struct Highest(u32);
///
/// impl Model for Highest {
///     type Msg = u32;
///     type Node = ();
///     type Action = ();
///     type Event = ();
///     fn new(_cx: &mut Cx<'_, Self>) -> Self {
///         Self(0)
///     }
///     fn deliver(&mut self, message: u32, _cx: &mut Cx<'_, Self>) {
///         self.0 = self.0.max(message);
///     }
/// }
///
/// let mut harness = Harness::<Highest>::new(1);
/// harness.send(7);
/// harness.send(3);
/// assert!(harness.deliver_where(|m| *m == 7)); // out of order, by hand
/// assert_eq!(harness.drain(10), 1);
/// assert_eq!(harness.model().0, 7);
/// ```
pub struct Harness<M: Model> {
    pub(crate) model: M,
    pub(crate) core: Core<M>,
}

impl<M: Model> Harness<M> {
    /// The model of `seed`, built by [`Model::new`].
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self::with_mailbox(seed, DEFAULT_MAILBOX)
    }

    /// The model of `seed`, with at most `bound` messages in flight.
    #[must_use]
    pub fn with_mailbox(seed: u64, bound: usize) -> Self {
        Self::build(seed, bound, M::new)
    }

    /// A model that `make` builds: a scripted test that needs one exact
    /// deployment, not the one the seed draws.
    pub fn build(seed: u64, bound: usize, make: impl FnOnce(&mut Cx<'_, M>) -> M) -> Self {
        let mut core = Core::new(seed, bound);
        let model = make(&mut Cx { core: &mut core });
        Self { model, core }
    }

    /// The model.
    #[must_use]
    pub fn model(&self) -> &M {
        &self.model
    }

    /// The model, to change its state by hand. No check runs afterwards.
    pub fn model_mut(&mut self) -> &mut M {
        &mut self.model
    }

    /// The generator of this run.
    pub fn rng(&mut self) -> &mut Rng {
        &mut self.core.rng
    }

    /// The messages in flight.
    #[must_use]
    pub fn mailbox(&self) -> &Mailbox<M::Msg> {
        &self.core.mailbox
    }

    /// The messages in flight, to reorder, filter or empty them by hand.
    pub fn mailbox_mut(&mut self) -> &mut Mailbox<M::Msg> {
        &mut self.core.mailbox
    }

    /// Call the model with a [`Cx`], as the walk does: for a helper of the
    /// model that sends or draws and is not one of its actions.
    /// [`Model::check`] runs afterwards.
    pub fn with<R>(&mut self, call: impl FnOnce(&mut M, &mut Cx<'_, M>) -> R) -> R {
        let mut cx = Cx {
            core: &mut self.core,
        };
        let result = call(&mut self.model, &mut cx);
        self.end_step();
        result
    }

    /// Everything done so far, in order.
    #[must_use]
    pub fn log(&self) -> &[Logged<M>] {
        &self.core.log
    }

    /// The counts so far.
    #[must_use]
    pub fn stats(&self) -> &Stats {
        &self.core.stats
    }

    /// The phase of the run.
    #[must_use]
    pub fn phase(&self) -> Phase {
        self.core.phase
    }

    /// Put `message` in flight, as a party of the model would.
    pub fn send(&mut self, message: M::Msg) {
        self.core.send(message);
    }

    /// Hand `message` to the model now, without the mailbox.
    pub fn deliver(&mut self, message: M::Msg) {
        self.deliver_unchecked(message);
        self.end_step();
    }

    /// Deliver the oldest message in flight. `false` when there is none.
    pub fn deliver_next(&mut self) -> bool {
        let message = self.core.mailbox.pop_front();
        self.deliver_taken(message)
    }

    /// Deliver a random message in flight. `false` when there is none.
    pub fn deliver_random(&mut self) -> bool {
        let message = self.core.mailbox.take(&mut self.core.rng);
        self.deliver_taken(message)
    }

    /// Deliver the oldest message in flight that `wanted` admits. `false`
    /// when there is none.
    pub fn deliver_where(&mut self, wanted: impl FnMut(&M::Msg) -> bool) -> bool {
        let message = self.core.mailbox.take_where(wanted);
        self.deliver_taken(message)
    }

    /// Lose every message in flight that `lost` admits: a scripted
    /// partition. Returns how many were lost.
    pub fn drop_where(&mut self, lost: impl FnMut(&M::Msg) -> bool) -> usize {
        self.core.drop_where(lost)
    }

    /// Deliver messages oldest first, the ones sent on the way too, until
    /// the mailbox is empty or `max` are delivered. Returns how many were
    /// delivered.
    pub fn drain(&mut self, max: usize) -> usize {
        let mut count = 0;
        while count < max && self.deliver_next() {
            count += 1;
        }
        count
    }

    /// Make one move of the model.
    pub fn act(&mut self, action: M::Action) {
        self.act_unchecked(action);
        self.end_step();
    }

    /// Crash `node`.
    pub fn crash(&mut self, node: &M::Node) {
        self.crash_unchecked(node);
        self.end_step();
    }

    /// Restart `node`.
    pub fn restart(&mut self, node: &M::Node) {
        self.restart_unchecked(node);
        self.end_step();
    }

    /// Stop the faults: enter the quiet phase, call [`Model::calm`], then
    /// ask every node that is down to restart.
    /// [`Walk::finish`](crate::Walk::finish) runs the whole tail of a walk
    /// instead.
    pub fn calm(&mut self) {
        self.calm_unchecked();
        self.end_step();
    }

    /// The run so far, as a [`Run`].
    #[must_use]
    pub fn into_run(self) -> Run<M> {
        Run {
            seed: self.core.seed,
            model: self.model,
            log: self.core.log,
            stats: self.core.stats,
        }
    }

    fn deliver_taken(&mut self, message: Option<M::Msg>) -> bool {
        let Some(message) = message else {
            return false;
        };
        self.deliver(message);
        true
    }

    /// Count the step and run the oracles.
    pub(crate) fn end_step(&mut self) {
        self.model.check();
        self.count_step();
    }

    pub(crate) fn count_step(&mut self) {
        self.core.step += 1;
        self.core.stats.steps += 1;
    }

    pub(crate) fn deliver_unchecked(&mut self, message: M::Msg) {
        self.core.stats.delivered += 1;
        self.core.note(Entry::Deliver(message.clone()));
        let mut cx = Cx {
            core: &mut self.core,
        };
        self.model.deliver(message, &mut cx);
    }

    pub(crate) fn act_unchecked(&mut self, action: M::Action) {
        if let Some(index) = M::actions().iter().position(|known| *known == action)
            && let Some(count) = self.core.stats.actions.get_mut(index)
        {
            *count += 1;
        }
        self.core.note(Entry::Act(action));
        let mut cx = Cx {
            core: &mut self.core,
        };
        self.model.act(action, &mut cx);
    }

    pub(crate) fn crash_unchecked(&mut self, node: &M::Node) {
        self.core.stats.crashes += 1;
        self.core.note(Entry::Crash(node.clone()));
        let mut cx = Cx {
            core: &mut self.core,
        };
        self.model.crash(node, &mut cx);
    }

    pub(crate) fn restart_unchecked(&mut self, node: &M::Node) {
        self.core.note(Entry::Restart(node.clone()));
        let mut cx = Cx {
            core: &mut self.core,
        };
        self.model.restart(node, &mut cx);
        if self.model.is_up(node) {
            self.core.stats.restarts += 1;
        }
    }

    pub(crate) fn calm_unchecked(&mut self) {
        self.core.enter(Phase::Quiet);
        self.core.note(Entry::Calm);
        let mut cx = Cx {
            core: &mut self.core,
        };
        self.model.calm(&mut cx);
        self.revive();
    }

    /// Ask every node that is down to restart. A node that came up is
    /// logged, after what its restart sent; a node that refused is not.
    pub(crate) fn revive(&mut self) {
        for node in self.model.nodes() {
            if self.model.is_up(&node) {
                continue;
            }
            let mut cx = Cx {
                core: &mut self.core,
            };
            self.model.restart(&node, &mut cx);
            if self.model.is_up(&node) {
                self.core.stats.restarts += 1;
                self.core.note(Entry::Restart(node));
            }
        }
    }
}
