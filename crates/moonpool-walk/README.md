# moonpool-walk

A seeded random walk over sans-I/O state machines.

A sans-I/O library is a set of pure state machines: a message comes in,
writes and messages come out. This crate drives such machines with no
executor, no network and no clock. A seed draws a schedule: deliver a
message, drop it, duplicate it, reorder it, crash a node, restart it from
its disk, or let the model make one of its own moves. The oracles of the
model run after every step. A run takes microseconds, so a campaign of
thousands of seeds is a unit test.

It is the rung below a `moonpool-sim` simulation. The simulation runs the
real I/O code against a simulated network and disk. The walk runs only the
protocol core, and judges it faster and more harshly.

The crate depends on `moonpool-assertions` and `moonpool-buggify` only, which
have no dependencies. It compiles on `wasm32-unknown-unknown`, macOS and
Linux.

## You write a model

```rust
use moonpool_walk::{Cx, Model, Phase, Walk};

struct Retries { /* the state machines, their disks, the oracle's notes */ }

impl Model for Retries {
    type Msg = Msg;        // a message in flight
    type Node = ();        // a party that can crash
    type Action = Action;  // a move of the model: a request, a tick
    type Event = u32;      // a fact for a trace checker

    fn new(cx: &mut Cx<'_, Self>) -> Self { /* draw the deployment */ }
    fn actions() -> &'static [Action] { &[Action::Submit, Action::Retry] }
    fn weight(action: Action, phase: Phase) -> u32 { /* 0 keeps a move out of a phase */ }
    fn act(&mut self, action: Action, cx: &mut Cx<'_, Self>) { /* cx.send(..) */ }
    fn deliver(&mut self, message: Msg, cx: &mut Cx<'_, Self>) { /* cx.send(..), cx.record(..) */ }
    fn nodes(&self) -> Vec<()> { vec![()] }
    fn is_up(&self, node: &()) -> bool { self.up }
    fn crash(&mut self, node: &(), cx: &mut Cx<'_, Self>) { /* keep the disk */ }
    fn restart(&mut self, node: &(), cx: &mut Cx<'_, Self>) { /* read the disk */ }
    fn check(&mut self) { /* assert!(..): safety, after every delivery and step */ }
    fn converged(&self) -> Result<(), String> { /* liveness, after the quiet tail */ }
}

#[test]
fn retries_apply_once() {
    Walk::<Retries>::new().seeds(300).run();
}
```

The complete example is in the crate documentation. `tests/paxos.rs` is a
single-decree Paxos with a mutant that the walk catches.

## The walk owns the schedule

| Phase | What happens |
|-------|--------------|
| Chaos | Each step is one draw by weight: deliver messages in random order (each may be dropped or duplicated), crash a node, restart a node, or one action of the model. |
| Quiet | The faults stop. At each step every node that is down is asked to restart. The walk keeps delivering and drawing the actions that have a weight in this phase. |
| Settle | Rounds of "every action of this phase once, then empty the mailbox" until `converged()` holds. A run that never converges fails with the reason `converged()` gave. |

## What you get on every model

- **Replay.** A failure prints the seed, the step, the last entries of the
  log and the state of the model. `MOONPOOL_WALK_SEED=17 cargo test` runs
  that seed alone, `MOONPOOL_WALK_TAIL=100000` prints its whole log, and
  `MOONPOOL_WALK_SEEDS=5000` makes a long run. Every walk with the default
  prefix reads these: `Walk::env_prefix` gives a model its own names.
- **Coverage.** A `sometimes!` or `reachable!` site that no seed passes
  fails the campaign. `Model::reachable_sites` and `Model::sometimes_sites`
  declare the sites that may never be evaluated.
- **Always assertions.** An always-style assertion that the code under test
  records in the `moonpool-assertions` table fails the seed, at the step
  that records it.
- **Buggify.** The walk seeds `moonpool-buggify`, so `buggify!` sites in the
  code under test fire. `buggify_fault_with_prob!` and `buggify_named!`
  sites are silent in the quiet tail.
- **Swarm testing.** `Walk::swarm(true)` switches off a random subset of the
  fault families and of the actions for each seed.
- **Mutation.** `Walk::try_run` returns the failure, so a test proves that a
  known bug is caught.
- **A trace.** `Cx::record` keeps the facts of a run in order.
  `Walk::run_each` hands each `Run` to a trace checker, a TLA+ one for
  example.
- **Scripted schedules.** `Harness` is the same engine one step at a time: a
  schedule that a seed found becomes a named regression test, and
  `Walk::finish` ends it with the quiet tail of a walk.
- **A determinism check.** `Walk::check_determinism` runs a seed twice and
  compares the logs.

## Rules for a model

Draw every choice from `Cx::rng`. Do not read a clock. Do not iterate a
`HashMap`: use a `BTreeMap`, or a seed will not replay.
