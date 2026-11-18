# A Random Walk over Pure State Machines

<!-- toc -->

`moonpool-walk` tests the part of a system that does no I/O. A sans-I/O
library is a set of pure state machines: a message comes in, writes and
messages come out. The walk drives these machines directly. There is no
executor, no network and no clock. A seed draws a schedule, and the oracles
of the model run after every step.

It is not a small simulator. A `moonpool-sim` simulation runs your real
networking and storage code against a simulated world. The walk runs only
the protocol core, so one run takes microseconds, and a campaign of thousands
of seeds is a unit test that runs with `cargo nextest run`.

## Where It Sits

A protocol is judged by several tools, from the most abstract to the most
real:

1. A **model** (TLA+ for example) checks the design.
2. A **trace check** compares what the code did with the model.
3. A **random walk** drives the real state machines through adversarial
   schedules. This chapter.
4. **Mutation** proves that the judges catch a known bug.
5. A **simulation** runs the whole system, I/O included.

The walk is harsher than the simulation on one point. The simulated network
is TCP: a connection delivers in order or breaks. The walk has a bag of
messages: any message can arrive at any step, in any order, any number of
times, or never. A protocol core must be safe under that network.

## A Model

You write one type that implements `Model`. It owns the state machines, their
disks and the oracles. Here a sender retries until the receiver acknowledges,
and the receiver must apply each request once.

```rust,ignore
use moonpool_walk::{Cx, Model, Phase, Walk};

#[derive(Clone, Debug)]
enum Msg { Put(u32), Ack(u32) }

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action { Submit, Retry }

struct Retries {
    next: u32,
    pending: Vec<u32>,
    applied: Vec<u32>, // on the receiver's disk
    up: bool,
}

impl Model for Retries {
    type Msg = Msg;
    type Node = ();
    type Action = Action;
    type Event = u32;

    fn new(_cx: &mut Cx<'_, Self>) -> Self {
        Self { next: 0, pending: Vec::new(), applied: Vec::new(), up: true }
    }

    fn actions() -> &'static [(Action, u32)] {
        &[(Action::Submit, 10), (Action::Retry, 20)]
    }

    fn act(&mut self, action: Action, cx: &mut Cx<'_, Self>) {
        match action {
            Action::Submit if cx.phase() == Phase::Chaos => {
                self.pending.push(self.next);
                cx.send(Msg::Put(self.next));
                self.next += 1;
            }
            Action::Submit => {} // no new request in the quiet tail
            Action::Retry => {
                for id in &self.pending {
                    cx.send(Msg::Put(*id));
                }
            }
        }
    }

    fn deliver(&mut self, message: Msg, cx: &mut Cx<'_, Self>) {
        match message {
            Msg::Put(_) if !self.up => {} // lost: the receiver is down
            Msg::Put(id) => {
                if !self.applied.contains(&id) {
                    self.applied.push(id);
                    cx.record(id);
                }
                cx.send(Msg::Ack(id));
            }
            Msg::Ack(id) => self.pending.retain(|p| *p != id),
        }
    }

    fn nodes(&self) -> Vec<()> { vec![()] }
    fn is_up(&self, _node: &()) -> bool { self.up }
    fn crash(&mut self, _node: &(), _cx: &mut Cx<'_, Self>) { self.up = false; }
    fn restart(&mut self, _node: &(), _cx: &mut Cx<'_, Self>) { self.up = true; }

    fn check(&mut self) {
        let mut seen = self.applied.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), self.applied.len(), "a request is applied once");
    }

    fn converged(&self) -> bool {
        self.pending.is_empty()
    }
}

#[test]
fn retries_apply_once() {
    Walk::<Retries>::new().seeds(300).run();
}
```

Only `new` and `deliver` are required. The four associated types name the
alphabet of the model:

| Type | What it is | When there is none |
|------|------------|--------------------|
| `Msg` | a message in flight | every model has one |
| `Node` | a party that can crash | `()` and leave `nodes()` empty |
| `Action` | a move of the model: a request, a tick, a step-down | `()` and leave `actions()` empty |
| `Event` | a fact recorded for a trace checker | `()` |

There is no clock. Time passes when the walk draws a tick action. A timeout
is a move of the model like any other, and the walk decides when it happens.

## The Schedule

The walk owns the messages in flight and decides what happens next. A run has
three phases.

| Phase | What the walk does |
|-------|--------------------|
| **Chaos** | Each step is one draw by weight: deliver three messages in random order, crash a node that is up, restart a node that is down, or one action of the model. A message taken for delivery is dropped with probability 0.1. A delivered message goes back in flight with probability 0.1. |
| **Quiet** | The faults stop. `Model::calm` runs once, then every node that is down is asked to restart. Nothing is dropped or duplicated. The walk keeps delivering and drawing actions. |
| **Settle** | Each round runs every action once and then empties the mailbox. The rounds stop when `Model::converged` holds. A run that does not converge in 200 rounds fails. |

`Model::check` runs after every step, in every phase. The mailbox holds at
most 128 messages. One more evicts a random message, in every phase: this is
the limit that stops a backlog from starving the work.

The model holds the truth about who is up. The walk asks `is_up` and keeps no
list of its own, so a node may also crash by itself inside `deliver`, between
a write and its sync for example. A model may refuse a restart: a node that
stays dead for the tail is how you prove that the others take its work over.

Every number is a method of `Walk`:

```rust,ignore
Walk::<ProxyModel>::new()
    .seeds(300)
    .chaos_steps(600)
    .quiet_steps(400)
    .drop_probability(0.2)
    .crash_weights(5, 5)
    .max_down(1)
    .mailbox(64)
    .env_prefix(Some("PROXY_MODEL"))
    .run();
```

## When a Seed Fails

A failed `assert!` anywhere in the model fails the seed. The campaign stops
and prints what you need to run it again:

```text
seed 2817 failed at step 43: at most one value is chosen: {2, 3}
run it again alone with PAXOS_WALK_SEED=2817
the last 40 entries of the log:
    ...
    42 chaos  deliver Promise { to: 1, from: 0, ballot: 7, accepted: Some((2, 2)) }
    43 chaos  deliver Accept { to: 1, ballot: 7, value: 3 }
    43 chaos  duplicate Accept { to: 2, ballot: 7, value: 3 }
    43 chaos  deliver Accept { to: 2, ballot: 7, value: 3 }
    43 chaos  event 3
the model:
acceptor 0: promised 7 accepted Some((2, 2)) down false
acceptor 1: promised 7 accepted Some((7, 3)) down false
acceptor 2: promised 7 accepted Some((7, 3)) down false
```

The walk reads four environment variables. With the default prefix they are
`MOONPOOL_WALK_SEED` (one seed alone), `MOONPOOL_WALK_SEEDS` (how many),
`MOONPOOL_WALK_FROM` (the first seed) and `MOONPOOL_WALK_STEPS` (the chaos
steps). `env_prefix` gives each model its own names, so two models in one
test binary do not share them.

`Walk::replay(seed)` returns the `Run` of one seed: the model as the run left
it, and the log of everything the run did.

A seed replays only when the model draws every choice from `cx.rng()`. A
model that reads a clock or iterates a `HashMap` does not replay.
`Walk::check_determinism(seed)` runs a seed twice and compares the two logs.

## Coverage: Sometimes Assertions

A walk that never reaches the hard case proves nothing about it. Mark the
cases with `sometimes!` and `reachable!`, the probes of
[`moonpool-assertions`](../part3-building/12-assertions.md):

```rust,ignore
moonpool_walk::sometimes!(
    inherited.is_some(),
    "a proposer adopted a value accepted at a lower ballot"
);
```

The counts add up over the campaign. A site that no seed passes fails the
campaign with a coverage error, after every seed passed. A site that is never
evaluated is invisible, so declare the ones that matter with
`Walk::expect_sometimes("...")`. The `Report` lists every site with its hit
count.

An always-style assertion that the code under test records in the table
(`assertion_bool` with `AssertKind::Always`) fails the seed that records it.

Coverage is not judged when one seed runs alone.

## Buggify and Swarm Testing

For each seed the walk installs a seeded source into
[`moonpool-buggify`](../part3-building/08-buggify.md). The `buggify!` sites
in the code under test fire as they do in a simulation: a location is active
on half the seeds, and an active location fires on a quarter of its calls.
When the chaos phase ends the walk calls `buggify_enter_recovery`, so the
fault sites are silent in the tail. Buggify draws from its own stream:
adding a site does not move the schedule of a seed.

`Walk::swarm(true)` is swarm testing at the level of the schedule. Each seed
switches off a random subset of the fault families (drops, duplicates,
crashes) and of the actions of the model for its chaos phase. A seed with no
crash goes deeper into the protocol. A seed with no client request makes
elections collide. The quiet tail always runs every action.

## Mutation: Prove That It Bites

A campaign that passes proves little until you see it fail. `Walk::try_run`
returns the failure as a value, so a test can run a mutant and demand that it
is caught:

```rust,ignore
let error = Walk::<Paxos<true>>::new() // acceptors that forget their promise
    .seeds(5_000)
    .try_run()
    .expect_err("the mutant chooses two values on some seed");
let WalkError::Seed(failure) = error else { panic!("{error}") };
assert!(failure.message.contains("at most one value is chosen"));
```

`crates/moonpool-walk/tests/paxos.rs` is this test in full: a single-decree
Paxos with three acceptors and two proposers. The correct protocol passes
every seed, converges after the tail, and reaches its three `sometimes!`
sites. The mutant keeps its promise in memory only, and the walk finds the
seed where two values are chosen: seed 2817 of 5,000, in about one second.

## A Scripted Schedule

`Harness` is the engine of the walk, one step at a time. When a seed finds a
bug, write its schedule by hand and keep it as a named test:

```rust,ignore
let mut harness = Harness::<Paxos<false>>::new(1);
harness.act(Action::Propose);
harness.deliver(Msg::Prepare { to: 1, from: 0, ballot: 13 });
harness.crash(&1);
harness.restart(&1);
harness.deliver(Msg::Accept { to: 1, ballot: 2, value: 7 });
assert_eq!(harness.model().acceptors[1].accepted, None, "the promise is durable");
```

`deliver_next`, `deliver_where`, `drop_where` and `drain` move the messages
in flight in the order you choose. `Model::check` runs after each call.

## A Trace for a Checker

`cx.record(event)` keeps a fact of the run, in order. `Run::events` returns
them, and `Walk::run_each` hands every finished run to a closure:

```rust,ignore
Walk::<Paxos<false>>::new().run_each(|run| {
    let mut trace = Trace::new();
    for event in run.events() {
        trace.event(event.name(), event.fields());
    }
    check_against_the_spec(&trace);
})?;
```

The walk knows no trace format. The event type is yours, and the conversion
to a checker's records is a few lines in the test. `Run::log` also holds the
steps of the walk itself (deliver, drop, duplicate, crash, restart), for a
spec that models the faults.

## What the Walk Does Not Do

- It does not run async code. A state machine that awaits is a job for the
  [simulation](../part3-building/01-first-simulation.md).
- It does not model a partition. A model that needs one adds an action that
  cuts a link and drops the matching messages in `deliver`.
- It does not shrink a failing schedule. The log of a few hundred steps is
  short, and `Harness` turns the part that matters into a test.
- It is not exhaustive. A random walk samples schedules. A model checker
  enumerates them.
