---
name: discover-properties
description: Systematically find where a moonpool Process, Workload or Invariant needs assertions, coverage claims and buggify sites, using eight attention focuses (state integrity, concurrency, crash recovery, network faults, timing, resource bounds, protocol contracts, lifecycle). Use when starting a new simulation, when asked "what should we assert", when reviewing coverage gaps after a feature lands, or after a seed failure to find the sibling bugs. Fan out with the property-hunter agent for an independent pass per focus.
argument-hint: [file or module to examine]
---

# Discover properties

The goal is a list of concrete assertion and injection placements, each with
a file:line, a macro, a stable message and the bug it would catch. Work one
focus at a time and do not skip a focus because an earlier pass "covered"
the area: the value is in the independent angle.

## Ensemble mode (preferred when the area is more than one file)

Spawn one `property-hunter` agent per focus with the target path and the
focus name; each returns placements with a confidence. Then merge: a
placement found by several focuses is high confidence; one found by a single
focus is often the most valuable catch; resolve conflicts by asking which
macro's failure would be a *finding* (`/using-assertions`).

## The eight focuses

| Focus | Look for |
|---|---|
| **State integrity** | write-ordering assumptions a crash can split; monotonic fields (ballots, sequence ids); derived state that can drift from its source; reference model vs actual |
| **Concurrency** | check-then-act across an `.await`; two workloads on one key; shared state mutated by another task mid-operation |
| **Crash recovery** | in-memory state that should have gone through `ctx.storage()`; write without `sync_all`; recovery that assumes no torn write; `CrashAndWipe` vs `Crash` |
| **Network faults** | RPCs without timeout; retries that are not idempotent; stale peer/leader caches after `PartitionRestore`; ordering assumptions between messages; fire-and-forget sends that matter |
| **Timing & scheduling** | hard-coded timeouts that interact; timer-vs-delivery races; "enough time passed" without a fact; overlapping election/lease windows |
| **Resource boundaries** | unbounded `Vec`/`VecDeque` under load; missing backpressure; a queue depth that should be a `buggify_knob!` |
| **Protocol contracts** | guarantees stated in docs but never asserted; responses that mask partial failure; ordering between RPCs; serde-accepted type mismatches |
| **Lifecycle transitions** | requests before init; listener bound before state loaded; in-flight work dropped on `ctx.shutdown()`; `ctx.state().publish` of a not-yet-valid value |

## Output format

For each property:

```
[path:line-range] — one-line description
Macro:     assert_always! | assert_sometimes! | assert_reachable! | invariant (cross-process)
Message:   short, stable, no interpolated ids
Rationale: the bug it catches, in one sentence
Focus:     which focus(es) surfaced it
```

For each injection point:

```
[path:line-range] — one-line description
Pattern:     error injection | delay | knob (default, lo..hi, floor) | alternative path | restart
Probability: high (5-10%) | medium (1%) | low (0.1-0.01%)
Rationale:   the failure mode it makes likely
```

Placement rules: a per-process fact goes in the `Process`, a client-visible
fact in the `Workload`, a cross-process fact in an `Invariant` over tracing
events (`/events-and-invariants`); every injection point gets a
branch-guarded `assert_reachable!` (`/using-buggify`); a `sometimes` names
an outcome, never a perturbation.

Book: `book/src/part3-building/24-discovering-properties.md`,
`19-designing-workloads.md`.
