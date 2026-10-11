# Assertion Reference

<!-- toc -->

All macros live in `moonpool_sim` (also through the `moonpool` facade with the `sim` feature). Accounting lives in `moonpool-assertions`, a dependency-free crate with a heap table by default and a `MAP_SHARED` table under exploration. No macro panics. A violated always-style assertion records a failure for the current seed, and the report lists it.

## Macro Summary

| Macro | Form | Kind | Fails the seed when | Guides the explorer |
|-------|------|------|---------------------|---------------------|
| `assert_always!` | `(cond, msg)` or `(cond, msg, { "k" => v, .. })` | `Always` | `cond` is false, or the site never ran | No |
| `assert_always_or_unreachable!` | `(cond, msg)` or `(cond, msg, {..})` | `AlwaysOrUnreachable` | `cond` is false | No |
| `assert_sometimes!` | `(cond, msg)` | `Sometimes` | coverage miss: ran, never true | Yes: first pass |
| `assert_reachable!` | `(msg)` | `Reachable` | coverage miss: never reached | Yes: first pass |
| `assert_unreachable!` | `(msg)` or `(msg, {..})` | `Unreachable` | reached at all | No |
| `assert_always_greater_than!` | `(val, thresh, msg [, {..}])` | `NumericAlways` | `val > thresh` is false | No |
| `assert_always_greater_than_or_equal_to!` | same | `NumericAlways` | `val >= thresh` is false | No |
| `assert_always_less_than!` | same | `NumericAlways` | `val < thresh` is false | No |
| `assert_always_less_than_or_equal_to!` | same | `NumericAlways` | `val <= thresh` is false | No |
| `assert_sometimes_greater_than!` | `(val, thresh, msg)` | `NumericSometimes` | coverage miss: `val > thresh` never true | Yes: watermark |
| `assert_sometimes_greater_than_or_equal_to!` | same | `NumericSometimes` | coverage miss | Yes: watermark |
| `assert_sometimes_less_than!` | same | `NumericSometimes` | coverage miss | Yes: watermark |
| `assert_sometimes_less_than_or_equal_to!` | same | `NumericSometimes` | coverage miss | Yes: watermark |
| `assert_sometimes_all!` | `(msg, [(name, bool), ..])` | `BooleanSometimesAll` | coverage miss: never all true at once | Yes: frontier, combination |
| `assert_sometimes_each!` | `(msg, [(key, val), ..])` or `(msg, [identity..], [quality..])` | bucket table | never (no contract) | Yes: bucket, quality |

Rules that apply to every macro:

- `msg` is any `Display` value. It identifies the site, so keep it stable and unique.
- Numeric values go through `to_i64_saturating`, so any integer type works.
- The detail map `{ "k" => v, .. }` prints `[ASSERTION FAILED] msg (seed=N) | k=v ...` on failure only. Values must implement `Display`.
- `assert_sometimes_each!` takes at most 6 identity keys per site (`MAX_EACH_KEYS`). The quality list is optional.

## Numeric Watermarks

Every numeric site keeps one watermark, the best value seen for its direction:

| Macro family | Watermark tracks |
|--------------|------------------|
| `assert_always_greater_than[_or_equal_to]!` | the minimum value (the worst case) |
| `assert_always_less_than[_or_equal_to]!` | the maximum value (the worst case) |
| `assert_sometimes_greater_than[_or_equal_to]!` | the maximum value |
| `assert_sometimes_less_than[_or_equal_to]!` | the minimum value |

Always-style watermarks are reporting only. An improvement on a `NumericSometimes` watermark is a `WatermarkImprovement` discovery, so the explorer branches from it.

## Discovery Kinds

The accounting layer fires `on_discovery(kind, state_id)` at most once per distinct discovery across every process that shares the table. The `DiscoveryKind` values and the macros that produce them:

| Kind | Produced by | Explorer priority |
|------|-------------|-------------------|
| `WatermarkImprovement` | a `NumericSometimes` watermark moves | progress (2) |
| `FrontierAdvance` | more `assert_sometimes_all!` facts true at once than before | progress (2) |
| `BucketQuality` | an `assert_sometimes_each!` bucket's quality value improves | progress (2) |
| `BucketFirst` | a new `assert_sometimes_each!` identity bucket | structured novelty (1) |
| `BooleanCombination` | a new partial truth combination in `assert_sometimes_all!` | structured novelty (1) |
| `SometimesPass` | first pass of `assert_sometimes!` or `assert_reachable!` | coverage (0) |

## Contract Validation

`validate_assertion_contracts()` returns `(always_violations, coverage_violations)` after the campaign:

| Kind | Always violation | Coverage violation |
|------|------------------|--------------------|
| `Always` | `fail_count > 0`, or the site was never evaluated (`must_hit`) | |
| `AlwaysOrUnreachable` | `fail_count > 0` | |
| `Unreachable` | `pass_count > 0` | |
| `NumericAlways` | `fail_count > 0` | |
| `Sometimes` | | evaluated at least once, never passed |
| `Reachable` | | never reached |
| `NumericSometimes` | | evaluated, never passed |
| `BooleanSometimesAll` | | `frontier < frontier_target` |

Table integrity counts as an always violation: dropped slot allocations, dropped bucket allocations, a slot conflict or a bucket conflict (two messages hashing to one slot). `SimulationReport.dropped_assertion_allocations` reports the dropped count.

Coverage violations do not fail a `FixedCount` campaign by themselves. They stop `UntilCoverageStable` from converging, and the report prints them.

## Table Limits

| Constant | Value | Meaning |
|----------|-------|---------|
| `MAX_ASSERTION_SLOTS` | 2048 | distinct assertion sites per campaign |
| `MAX_EACH_BUCKETS` | 256 | distinct `assert_sometimes_each!` identity buckets |
| `MAX_EACH_KEYS` | 6 | identity keys per `assert_sometimes_each!` call |

Bucket a coarse vocabulary (phase, role, floor), not identifiers (node id, slot number).

## Probe Macros

`moonpool-assertions` exports two lightweight probes for library code that must not depend on `moonpool-sim`:

| Macro | Form | Equivalent |
|-------|------|------------|
| `reachable!` | `reachable!("msg")` | `assert_reachable!` |
| `sometimes!` | `sometimes!(cond, "msg")` | `assert_sometimes!` |

`moonpool-sim` re-exports them as `moonpool_sim::reachable` and `moonpool_sim::sometimes`.

## Related Functions and Types

| Item | Where | Purpose |
|------|-------|---------|
| `assertion_results() -> BTreeMap<String, AssertionStats>` | `moonpool_sim` | per-message `{ total_checks, successes }` |
| `reset_assertion_results()` | `moonpool_sim` | clear the table (the runner does this per campaign) |
| `has_always_violations()`, `reset_always_violations()` | `moonpool_sim` | the per-run always flag |
| `validate_assertion_contracts()` | `moonpool_sim` | the rules above |
| `AssertKind`, `AssertCmp` | `moonpool_assertions`, re-exported | the kind and comparison enums |
| `AssertionDetail` | `moonpool_sim` report | `msg, kind, pass_count, fail_count, watermark, frontier, frontier_target, combinations_seen, status` |
| `AssertionStatus` | `moonpool_sim` report | `Fail`, `Miss`, `Pass` |
| `BucketSiteSummary` | `moonpool_sim` report | `msg, buckets_discovered, total_hits` per `sometimes_each` site |
| `SimulationReport.assertion_details` | report | one `AssertionDetail` per slot |
| `SimulationReport.bucket_summaries` | report | one `BucketSiteSummary` per site |
| `SimulationReport.assertion_violations` | report | always-style violations found by validation |
| `SimulationReport.coverage_violations` | report | sometimes-style misses |

## Where to Use Which

| Situation | Macro |
|-----------|-------|
| An invariant that must hold on every evaluation | `assert_always!` |
| An invariant on a path that may never run under some seeds | `assert_always_or_unreachable!` |
| A branch the chaos must reach (retry, timeout, recovery) | `assert_sometimes!` or `assert_reachable!` |
| A branch that must never run | `assert_unreachable!` |
| A bound (queue depth, latency, replica lag) | `assert_always_less_than!` and friends |
| Depth the explorer should push (level reached, slots decided) | `assert_sometimes_greater_than!` |
| Several facts that must coincide at least once | `assert_sometimes_all!` |
| Coverage per identity with a quality value | `assert_sometimes_each!` |

See [Assertions: Finding Bugs](../part3-building/12-assertions.md) for the narrative and [The Frontier Controller](../part5-building-on-top/03-frontier-controller.md) for how discoveries drive exploration.
