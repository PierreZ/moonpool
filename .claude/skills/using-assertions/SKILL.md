---
name: using-assertions
description: Choose and write moonpool assertion macros - assert_always!, assert_sometimes!, assert_reachable!, assert_unreachable!, the always/sometimes numeric family, assert_sometimes_all! and assert_sometimes_each!, the detail-map form, and the 512-slot / 256-bucket budgets. Use whenever adding or editing any moonpool assertion, deciding between an invariant and a coverage claim, reading a coverage_violations list, or when a run reports a slot-table overflow. Not for plain Rust assert! in unit tests.
---

# Using assertions

Moonpool's assertions are Antithesis-style accounting, not `assert!`. **None
of them panic.** An `assert_always!` that fails records a violation and the
run continues, so a single root cause shows its whole cascade in one seed
and the report lists every message that failed. `assert_sometimes!` and
`assert_reachable!` never fail a seed at all; they fail the *campaign* when a
full sweep ends without satisfying them, which is how the harness proves it
reaches what it claims.

All fifteen are `#[macro_export]` from `crates/moonpool-sim/src/chaos/assertions.rs`
and used as `moonpool_sim::assert_always!`.

## Which one

| You want to say | Macro | Fails when |
|---|---|---|
| this must hold every time it is checked | `assert_always!(cond, msg)` / `(cond, msg, { "k" => v, .. })` | `cond` is false once |
| this must hold, but the path may never run | `assert_always_or_unreachable!(cond, msg)` | `cond` is false once |
| this line must never execute | `assert_unreachable!(msg)` / `(msg, { .. })` | it executes |
| the sweep must reach this outcome at least once | `assert_sometimes!(cond, msg)` | never true across the campaign |
| this branch must be reached at least once | `assert_reachable!(msg)` | never reached across the campaign |
| bound a value, hard | `assert_always_{greater_than, greater_than_or_equal_to, less_than, less_than_or_equal_to}!(val, thresh, msg)` | bound broken once |
| a value should sometimes cross a threshold (guides exploration by watermark) | `assert_sometimes_{greater_than, ..}!(val, thresh, msg)` | never crossed |
| several named booleans should all be true at once, eventually | `assert_sometimes_all!(msg, [("a", a), ("b", b)])` | the frontier never reaches all |
| explore each value of a small key space | `assert_sometimes_each!(msg, [("kind", k)])` (optional quality watermarks) | a bucket is never seen |

Rules of thumb that keep the taxonomy honest:

- A `sometimes` names an **outcome** the run must be proven to reach (a
  leader was elected after a partition, a node recovered through a snapshot).
  Its failure is a finding about the harness's reach.
- A `reachable` names a **cause** that fired (a buggify site, a knob extreme,
  an operation the client drew). Put it behind the branch so it creates no
  slot when the seed never takes the branch; that way it can never fail
  coverage for a perturbation the swarm did not draw.
- Never give a perturbation a `sometimes`: a gate on "did buggify fire"
  turns a tuned probability into a CI failure.
- Messages are stable, short, free of interpolated ids; dynamic context goes
  in the detail map (`{ "node" => ip, "term" => term }`), which is printed
  with the seed on failure.

## The budget

- **512 assertion slots** per campaign process (`MAX_ASSERTION_SLOTS`,
  `crates/moonpool-assertions/src/slots.rs`), shared with moonpool's own
  internal assertions. Identity is the hash of the message, truncated to 64
  bytes; a reworded message is a *new* slot and silently resets its
  saturation history.
- **256 `sometimes_each` buckets** and **6 keys** per each
  (`MAX_EACH_BUCKETS`, `MAX_EACH_KEYS`, `buckets.rs`). Never bucket on a
  slot number, a request id, a seed or a hash: the space is unbounded and the
  table fills in one run.
- Overflow is reported as `report.dropped_assertion_allocations` **and** as an
  always-violation ("assertion slot table overflowed"), so it fails the run.
  Count before adding.

## Where each kind lives

- Per-process facts (a local invariant of the server) go in the `Process`.
- Client-visible facts (reference model, acked-then-readable) go in the
  `Workload`'s `run`/`check`.
- Cross-process facts (one leader per term) go in an `Invariant` over
  captured tracing events (`/events-and-invariants`), which also uses these
  macros inside `observe`.

## Reading the report

`report.assertion_violations` lists failed always/unreachable messages;
`report.coverage_violations` lists sometimes/reachable never satisfied;
`assertion_results`, `assertion_details` and `bucket_summaries` carry the
per-slot counts. Under exploration, the first success of a `sometimes` and
every watermark improvement is a fork point, so a well-placed `sometimes`
also *steers* the search.

Book: `book/src/part3-building/12-assertions.md` through
`16-compound-assertions.md`; `appendix/01-assertion-reference.md`.
