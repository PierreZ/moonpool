# Checking Runs Against a TLA+ Spec

<!-- toc -->

An invariant checks one rule that you wrote in Rust. A TLA+ spec holds every rule of a protocol, and TLC can check them all. Model-based trace checking joins the two: **is this run of the real code a behaviour that the spec allows?** The run records its events in order. A small trace spec forces the real spec to follow those events. TLC searches for a behaviour of the spec that matches them. If none exists, the code did something the model forbids.

The method comes from Cirstea, Kuppe, Loillier and Merz, [Validating Traces of Distributed Programs Against TLA+ Specifications](https://arxiv.org/abs/2404.16075) (2024). Microsoft CCF and etcd's raft use it in CI. A simulation makes it easier than on a real cluster:

- **One total order for free.** A moonpool run is one thread with one sequence counter. The timeline is already a linear trace of every process, so there are no clocks to merge.
- **Atomic steps.** Between two awaits a simulated task runs alone. An event logged after a state change belongs to the same step as the change.
- **A rejected trace is a seed.** The seed replays the run with logs at any level.
- **Better traces.** Chaos, buggify and exploration produce the rare interleavings that ordinary tests never log.

## The Parts

| Part | Where | What it does |
|------|-------|--------------|
| `Trace`, `Value` | `moonpool-tla` | The events of one run, written as a TLA+ module: `Trace == << [event \|-> "rm_prepare", rm \|-> "r1"], ... >>` |
| `TraceCheck`, `Verdict` | `moonpool-tla` | Runs TLC on a trace spec and reads the verdict |
| `trace_from_timeline` | `moonpool_sim::observability::tla` | Builds a `Trace` from the captured timeline |
| the trace spec | your repository | Extends your spec and follows the trace, one event per step |

`moonpool-tla` has no dependency. It does not need the simulator: a sans-I/O harness can record events itself and check them the same way. Running TLC needs the `tlc` launcher, which the nix dev shell installs (nixpkgs `tlaplus`). Set `MOONPOOL_TLC` to use another one.

## Recording a Run

Emit a plain `tracing` event after each step, as for an invariant ([Events and Invariants](./17-events-and-invariants.md)). Give it what the spec needs to match the step: the parameters of the action, and the state of the actor after the step.

```rust
tracing::info!(rm = %rm, state = %"prepared", "rm_prepare");
```

The state after the step is a **projection**: a few values that pin down the model's variables. The 2024 paper measures why it matters. With only event names, TLC must guess every variable, the search explodes, and a wrong trace can even be accepted. With the state logged, the search stays linear in the length of the trace.

Log the **faults** too. If the harness drops or repeats a message, emit an event for it, so TLC does not have to guess where faults happened. Turn off the fault kinds that the spec does not model: a fault outside the model gives a rejection that is not a bug.

In `check()`, build the trace from the timeline and add what depends on the run, such as the set of nodes:

```rust
use moonpool_sim::observability::tla::trace_from_timeline;
use moonpool_tla::{TraceCheck, Value, Verdict};

let mut trace = trace_from_timeline(ctx.observability(), &["rm_prepare", "tm_commit", /* ... */]);
trace.define("TraceRM", Value::str_set(["r1", "r2", "r3"]));
let verdict = TraceCheck::new(spec_dir, "TwoPhaseTrace").check(&trace)?;
```

Each event becomes a record. Its name goes in the `event` field, its source (the `ip` of the actor) in the `source` field, and each `tracing` field in a field of the same name. TLC integers have 32 bits, so record a large id as a string (`%id`). Floats become strings.

## The Trace Spec

The trace spec extends your spec and the trace module. It adds one variable, `l`, the index of the next event to match. Each step is the spec action that the event names, with the event's parameters. The projection is checked with primed variables:

```tla
EXTENDS TwoPhase, Trace, Naturals, Sequences
VARIABLE l
e == Trace[l]

TraceInit == TPInit /\ l = 1

Step ==
  \/ e.event = "rm_prepare" /\ RMPrepare(e.rm) /\ rmState'[e.rm] = e.state
  \/ e.event = "tm_commit"  /\ TMCommit        /\ tmState' = e.state
  \/ e.event \in {"msg_drop", "msg_duplicate"} /\ UNCHANGED vars
  \* ... one line per event

TraceNext == l <= Len(Trace) /\ Step /\ l' = l + 1
TraceSpec == TraceInit /\ [][TraceNext]_<<l, vars>>
TraceUnfinished == l <= Len(Trace)
```

An action the code does not log can stay free: add `\/ l <= Len(Trace) /\ Hidden /\ UNCHANGED l` to `TraceNext`, and TLC tries it between two events. Each hidden action makes the search larger, so log what you can.

The configuration checks the spec's invariants and `TraceUnfinished`:

```
SPECIFICATION TraceSpec
CONSTANT RM <- TraceRM
INVARIANT Consistent
INVARIANT TraceUnfinished
```

`TraceUnfinished` is violated exactly when a behaviour matched every event. That violation is how an accepted trace shows. Put the spec's own invariants before it, so a broken invariant is reported first.

## Verdicts

| Verdict | What TLC did | What it means |
|---------|--------------|---------------|
| `Accepted` | Reached the end of the trace | The run is a behaviour of the spec, and every invariant held on the way |
| `Rejected(rejection)` | Explored every match and stopped before the end | The code took a step the spec forbids, or the spec lacks a step the code is right to take |
| `InvariantViolated { invariant, matched, state }` | Matched a prefix that breaks an invariant | A safety bug, seen through the model's own invariant |

A rejection names the first event that no behaviour matches (`rejection.index`) and the model state just before it. To learn the index in one TLC run, `TraceCheck` runs a small generated module that extends the trace spec and records the highest `l` that any behaviour reached. A second run on the matched prefix prints the state.

A rejection is a finding in both directions. The papers report that the first rejections are mostly gaps in the spec or in the mapping from code to spec. Fix those too: each one is a fact about the code that the spec did not say.

## Controls

An accepted trace proves little if the check cannot fail. Plant **mutants**: copies of the code with one bug each, mirroring a step the spec forbids. Each mutant must give a rejected trace on some seed. A mutant that passes means the trace logs too little to pin the spec down.

`crates/moonpool-sim/tests/two_phase_trace.rs` is the canonical example. A sans-I/O two-phase commit runs in a workload that drops and repeats messages. Each seed's trace is checked against Lamport's `TwoPhase` spec. Every seed of the correct code is accepted, and three mutants are each rejected:

- a coordinator that commits after the first vote;
- a resource manager that commits without the commit message;
- a coordinator that aborts after it committed.

The first mutant breaks no invariant when no resource manager aborts. The trace check still sees it, because committing before every vote is not a step of the spec.

## Cost and Limits

- **One run, not the code.** An accepted trace says that this run is allowed. Coverage comes from the number and the quality of the runs, which is the simulation's job.
- **About a second per trace.** Most of it is the start of the JVM. That is fine for tens of seeds in a test, and too slow for every seed of a long sweep: check a sample.
- **32-bit integers.** Record large ids as strings, and map ballots or terms to small ranks if the spec compares them.
- **A loose spec passes.** Trace checking cannot find a bug that the spec also allows. Keep TLC's own model checking for the spec.
