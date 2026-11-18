//! A seeded random walk over sans-I/O state machines.
//!
//! A sans-I/O library is a set of pure state machines: a message comes in,
//! writes and messages come out. This crate drives such machines with no
//! executor, no network and no clock. A seed draws a schedule: deliver a
//! message, drop it, duplicate it, reorder it, crash a node, restart it
//! from its disk, or let the model make one of its own moves. The model's
//! oracles run after every step. A run takes microseconds, so a campaign of
//! thousands of seeds is a unit test.
//!
//! It is the rung below a `moonpool-sim` simulation. The simulation runs
//! the real I/O code against a simulated network and disk. The walk runs
//! only the protocol core, and judges it faster and more harshly: a message
//! can arrive at any step, in any order, any number of times.
//!
//! ## The parts
//!
//! | Part | Role |
//! |------|------|
//! | [`Model`] | what you write: the state machines, their disks, the oracles |
//! | [`Cx`] | what the model gets when it is called: the generator, `send`, `record` |
//! | [`Walk`] | the seeded schedule and the campaign of seeds |
//! | [`Harness`] | the same engine one step at a time, for a scripted test |
//! | [`Run`], [`Logged`], [`Entry`] | everything one seed did, in order |
//! | [`Report`], [`WalkError`], [`SeedFailure`] | the result of a campaign |
//! | [`Rng`], [`Mailbox`] | the generator and the bag of messages in flight |
//!
//! ## A model
//!
//! A sender retries until the receiver acknowledges. The receiver must
//! apply each request once, even when the network duplicates it or the
//! receiver restarts.
//!
//! ```rust
//! use moonpool_walk::{Cx, Model, Phase, Walk};
//!
//! #[derive(Clone, Debug)]
//! enum Msg {
//!     Put(u32),
//!     Ack(u32),
//! }
//!
//! #[derive(Clone, Copy, Debug, PartialEq)]
//! enum Action {
//!     Submit,
//!     Retry,
//! }
//!
//! struct Retries {
//!     next: u32,
//!     pending: Vec<u32>,
//!     applied: Vec<u32>, // on the receiver's disk
//!     up: bool,
//! }
//!
//! impl Model for Retries {
//!     type Msg = Msg;
//!     type Node = ();
//!     type Action = Action;
//!     type Event = u32;
//!
//!     fn new(_cx: &mut Cx<'_, Self>) -> Self {
//!         Self { next: 0, pending: Vec::new(), applied: Vec::new(), up: true }
//!     }
//!
//!     fn actions() -> &'static [(Action, u32)] {
//!         &[(Action::Submit, 10), (Action::Retry, 20)]
//!     }
//!
//!     fn act(&mut self, action: Action, cx: &mut Cx<'_, Self>) {
//!         match action {
//!             // No new request once the faults stopped.
//!             Action::Submit if cx.phase() == Phase::Chaos => {
//!                 self.pending.push(self.next);
//!                 cx.send(Msg::Put(self.next));
//!                 self.next += 1;
//!             }
//!             Action::Submit => {}
//!             Action::Retry => {
//!                 for id in &self.pending {
//!                     cx.send(Msg::Put(*id));
//!                 }
//!             }
//!         }
//!     }
//!
//!     fn deliver(&mut self, message: Msg, cx: &mut Cx<'_, Self>) {
//!         match message {
//!             Msg::Put(_) if !self.up => {} // lost: the receiver is down
//!             Msg::Put(id) => {
//!                 if !self.applied.contains(&id) {
//!                     self.applied.push(id);
//!                     cx.record(id);
//!                 }
//!                 moonpool_walk::sometimes!(
//!                     self.applied.len() > 3,
//!                     "the receiver applied more than three requests"
//!                 );
//!                 cx.send(Msg::Ack(id));
//!             }
//!             Msg::Ack(id) => self.pending.retain(|p| *p != id),
//!         }
//!     }
//!
//!     fn nodes(&self) -> Vec<()> {
//!         vec![()]
//!     }
//!     fn is_up(&self, _node: &()) -> bool {
//!         self.up
//!     }
//!     fn crash(&mut self, _node: &(), _cx: &mut Cx<'_, Self>) {
//!         self.up = false;
//!     }
//!     fn restart(&mut self, _node: &(), _cx: &mut Cx<'_, Self>) {
//!         self.up = true; // `applied` is durable
//!     }
//!
//!     fn check(&mut self) {
//!         let mut seen = self.applied.clone();
//!         seen.sort_unstable();
//!         seen.dedup();
//!         assert_eq!(seen.len(), self.applied.len(), "a request is applied once");
//!     }
//!
//!     fn converged(&self) -> bool {
//!         self.pending.is_empty()
//!     }
//! }
//!
//! let report = Walk::<Retries>::new().seeds(50).chaos_steps(200).report();
//! assert_eq!(report.seeds, 50);
//! assert!(report.stats.dropped > 0 && report.stats.crashes > 0);
//! ```
//!
//! A seed that fails prints its number, the step, the last entries of the
//! log and the variable that runs it again alone
//! (`MOONPOOL_WALK_SEED=17 cargo test`).
//!
//! ## The judges a walk gives
//!
//! - **Safety**: [`Model::check`] after every step, with plain `assert!`.
//! - **Liveness**: [`Model::converged`] after a quiet tail with no fault.
//! - **Coverage**: a [`sometimes!`] or [`reachable!`] site that no seed
//!   passes fails the campaign. A walk that never reaches the hard case
//!   proves nothing about it.
//! - **Rare branches**: [`buggify!`] sites in the code under test fire from
//!   the seed.
//! - **Mutation**: [`Walk::try_run`] returns the failure, so a test can
//!   prove that a known bug is caught.
//! - **A trace**: [`Cx::record`] keeps the facts of a run in order, and
//!   [`Walk::run_each`] hands each [`Run`] to a trace checker.
//!
//! ## Rules for a model
//!
//! Draw every choice from [`Cx::rng`]. Do not read a clock, and do not
//! iterate a `HashMap`: use a `BTreeMap`, or a seed will not replay.
//! [`Walk::check_determinism`] runs a seed twice and compares.

#![deny(missing_docs)]

mod harness;
mod mailbox;
mod model;
mod report;
mod rng;
mod walk;

pub use harness::{DEFAULT_MAILBOX, Harness};
pub use mailbox::Mailbox;
pub use model::{Cx, Entry, Logged, Model, Phase, Run, Stats};
pub use report::{Report, SeedFailure, Site, WalkError};
pub use rng::Rng;
pub use walk::{DEFAULT_ENV_PREFIX, Walk};

pub use moonpool_assertions::{reachable, sometimes};
pub use moonpool_buggify::{buggify, buggify_with_prob};
