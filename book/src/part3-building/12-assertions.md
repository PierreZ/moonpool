# Assertions: Finding Bugs

<!-- toc -->

A simulation catches a process in a bad state. We have two choices: stop the run, or record the fact and keep going. Traditional tests stop. Moonpool records and continues.

## Why Assertions Never Crash

**The first thing that goes wrong is rarely the worst thing that goes wrong.** If a replica commits a stale value and the run aborts there, we never learn what the replication protocol does with it. Maybe two other nodes accept the stale value. Maybe the system looks healthy for another thousand steps before a user reads garbage. An early abort hides that cascade, and the cascade is where the damage is.

So moonpool follows the Antithesis rule: **assertions record violations and continue**. `assert_always!` with a false condition does three things:

1. It increments the fail counter of its slot in the assertion table.
2. It prints `[ASSERTION FAILED] <message>` to stderr the first time that slot fails.
3. It sets a thread-local flag that marks the current seed as failed.

The run then goes on. More assertions fire, more facts are recorded. When the iteration ends, the runner reads the flag (`has_always_violations()`), counts the seed in `failed_runs` and `seeds_failing`, and resets the flag for the next seed. Nothing panics.

## Two Jobs for One Line

Every assertion does two jobs.

**It checks correctness.** The table keeps a pass count and a fail count per assertion, so a report says "failed 3 times out of 41,200", not just "failed". That ratio tells you how hard the bug is to hit.

**It guides the search.** When a `sometimes`-type assertion passes for the first time, or a watermark improves, the accounting layer calls a discovery hook. Without exploration the hook is a no-op. With [exploration](../part5-building-on-top/01-exploration.md) enabled, moonpool-explorer records the RNG position of the discovery and forks new timelines from it. The same `assert_sometimes!` that proves a path was covered also tells the explorer "branch here".

You write the assertion once, for correctness. The framework gets the guidance for free.

## The Fifteen Macros

`moonpool_sim` exports fifteen assertion macros. They fall into four families.

| Family | Macros | Chapter |
|--------|--------|---------|
| Boolean | `assert_always!`, `assert_always_or_unreachable!`, `assert_sometimes!`, `assert_reachable!`, `assert_unreachable!` | [Always and Sometimes](./14-always-sometimes.md) |
| Numeric, always | `assert_always_greater_than!`, `assert_always_greater_than_or_equal_to!`, `assert_always_less_than!`, `assert_always_less_than_or_equal_to!` | [Numeric Assertions](./15-numeric-assertions.md) |
| Numeric, sometimes | `assert_sometimes_greater_than!`, `assert_sometimes_greater_than_or_equal_to!`, `assert_sometimes_less_than!`, `assert_sometimes_less_than_or_equal_to!` | [Numeric Assertions](./15-numeric-assertions.md) |
| Compound | `assert_sometimes_all!`, `assert_sometimes_each!` | [Compound Assertions](./16-compound-assertions.md) |

The next chapter sorts the same fifteen by **purpose**: invariant, discovery or guidance. The [Assertion Reference](../appendix/01-assertion-reference.md) has every signature in one table.

## Where the Counts Live

The macros in `moonpool_sim` are thin wrappers. The accounting is `moonpool-assertions`, a crate with zero dependencies that compiles to wasm. It keeps two fixed-size tables:

- The **slot table**: 2048 slots, one per assertion site. A slot's identity is the 64-bit FNV-1a hash of its message. Each slot holds the kind, the pass and fail counts, a numeric watermark, and the frontier of a compound assertion. The message itself is stored truncated to 64 bytes.
- The **bucket table**: 256 buckets for `assert_sometimes_each!`, keyed by message plus identity keys.

By default both tables live on the heap of the test process. Under exploration, moonpool-explorer replaces them with `MAP_SHARED` memory, so forked worker processes write into the same counts. The macros behave the same either way.

Three rules follow from "the message is the identity":

1. **Use a constant message.** `assert_always!(ok, format!("slot {slot}"))` compiles, but every distinct string claims a new slot. Keep `format!` for a failure branch that fires rarely, as the leader-election test does, and never put it on a hot path.
2. **One message, one kind.** An `assert_always!` and an `assert_sometimes!` that share a message would evaluate one site under the other's contract. The table flags the slot as conflicted instead, and the run fails with "assertion '...' is shared by call sites of different kinds".
3. **Never let the table overflow silently.** An evaluation that finds the table full is counted. `SimulationReport::dropped_assertion_allocations` reports the count, and `is_failure()` returns true when it is non-zero. A full table fails loudly instead of looking green.

The counts accumulate across seeds. `reset_assertion_results()` zeroes them once at the start of `run()`, not between seeds, so a `sometimes` that fired in seed 7 still counts as reached in seed 300.

## The Reporting Pipeline

When the last seed ends, the runner calls `validate_assertion_contracts()`. It returns two lists.

**Always violations** are definite bugs, valid at any iteration count:

- an `always`-type assertion failed at least once,
- an `assert_always!` was never reached (a `must_hit` site with zero evaluations),
- an `assert_unreachable!` was reached,
- the slot or bucket table dropped evaluations, or two sites collided.

**Coverage violations** are statistical and only mean something after enough seeds:

- an `assert_sometimes!` or numeric `sometimes` was evaluated but never passed,
- an `assert_reachable!` was never reached,
- an `assert_sometimes_all!` never had every proposition true at once.

Both lists are on `SimulationReport` as `assertion_violations` and `coverage_violations`. `assertion_results` maps every message to an `AssertionStats { total_checks, successes }` with a `success_rate()`, and `assertion_details` carries the per-slot breakdown the report prints:

```text
━━━ Assertions (4) ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ✓  always       "dungeon bug: treasure found on the ..."  1,000 pass  0 fail
  ✓  sometimes    "near treasure"                            412 / 1,000  (41.2%)
  ○  sometimes    "stairs without key"                         0 / 1,000  (0.0%)
  ✓  numeric      "dungeon level reached"                    1,000 pass  0 fail  best: 6

━━━ Coverage Gaps ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ○  assert_sometimes!('stairs without key') has 0% success rate (1000 checks)
```

`✓` is pass, `✗` is a contract failure, `○` is a coverage miss. `report.is_failure()` is true for failed seeds, always violations and dropped allocations. It is **not** true for coverage gaps: a CI job that wants coverage enforced checks `coverage_violations` and `convergence_timeout` as well.

## Assertions Decide When to Stop

By default a `SimulationBuilder` runs with `IterationControl::UntilCoverageStable { plateau_seeds: 10, max_iterations: 1000 }`. The assertions are the stop condition. After every seed the runner scans the slot table and asks two questions:

1. Has every observed `sometimes`, numeric `sometimes`, `reachable` and `sometimes_all` site been satisfied at least once?
2. Has the progress signal stayed flat for `plateau_seeds` consecutive seeds?

The progress signal is real LLVM sancov edge coverage when the binary is instrumented, which is what `cargo xtask sim run` builds. Under plain `cargo nextest` it falls back to the number of distinct reached coverage sites. Both signals only grow, so "unchanged for 10 seeds" means the run stopped finding new states.

When both answers are yes, the run is **saturated** and stops early. The report records the outcome in `SimulationReport::saturation` (`sometimes_hit`, `sometimes_total`, the signal, edge counts). If `max_iterations` arrives first, `convergence_timeout` is set and the report prints `Saturation FAILED`.

```rust
SimulationBuilder::new()
    .workload(ThreeAlwaysHitWorkload)
    // Stop after 3 quiet seeds, never run more than 100.
    .until_coverage_stable(3, 100)
    .run()?;
```

A rare `assert_sometimes!` keeps the run alive until it fires. A workload with no coverage assertion at all saturates on the plateau alone, with a warning, because there is nothing left to reach. `set_iterations(n)` switches to `FixedCount` and ignores all of this, which is what you want when [reproducing one seed](./21-reproducing.md).

## Probes in Shipped Code

Shipped code can mark a path too. `moonpool_assertions::reachable!("label")` and `moonpool_assertions::sometimes!(cond, "label")` are the production forms of `assert_reachable!` and `assert_sometimes!`, like FoundationDB's `CODE_PROBE`. Outside a simulation no table is installed and each call is one pointer read. Inside one, the probe shares a slot with a simulation assertion of the same message. Never reword a probe's message: the hash is its identity across campaigns.
