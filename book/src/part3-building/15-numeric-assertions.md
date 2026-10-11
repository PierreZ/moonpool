# Numeric Assertions

<!-- toc -->

Boolean assertions answer yes or no. Many properties of a distributed system are numbers: queue depth stays under a bound, a cluster reaches a level, latency drops below a target. Eight macros compare two integers. Four are invariants. Four are guidance, and they give the explorer a value to improve.

## The Eight Macros

| Macro | Checks | Kind | Watermark |
|-------|--------|------|-----------|
| `assert_always_greater_than!(v, t, msg)` | `v > t` | invariant | lowest `v` |
| `assert_always_greater_than_or_equal_to!(v, t, msg)` | `v >= t` | invariant | lowest `v` |
| `assert_always_less_than!(v, t, msg)` | `v < t` | invariant | highest `v` |
| `assert_always_less_than_or_equal_to!(v, t, msg)` | `v <= t` | invariant | highest `v` |
| `assert_sometimes_greater_than!(v, t, msg)` | `v > t` | guidance | highest `v` |
| `assert_sometimes_greater_than_or_equal_to!(v, t, msg)` | `v >= t` | guidance | highest `v` |
| `assert_sometimes_less_than!(v, t, msg)` | `v < t` | guidance | lowest `v` |
| `assert_sometimes_less_than_or_equal_to!(v, t, msg)` | `v <= t` | guidance | lowest `v` |

Both operands go through `to_i64_saturating`. Any integer type with `TryInto<i64>` works, so a `u8`, a `usize` or a `u64` needs no cast. A value that does not fit becomes `i64::MAX`. Floats do not convert: scale a latency to milliseconds or microseconds first.

## Always: Bounds That Never Break

The four `assert_always_*` macros are invariants with a comparison instead of a bool.

```rust
assert_always_less_than_or_equal_to!(
    queue.len(),
    MAX_QUEUE,
    "the request queue never exceeds its bound",
    { "ip" => ctx.my_ip(), "depth" => queue.len() }
);
```

A failed comparison records a fail, marks the seed failed, and the run continues. The first failure of a slot prints `[NUMERIC ASSERTION FAILED] <msg> (left=.., right=.., cmp=Le)`. With a details block, every failure prints the operands, the seed and your values. The report lists the slot under `num-always` with `N pass  M fail  best: W`.

`best` is the **watermark**: the most extreme value of the left operand seen in the whole run. For a `less_than` bound it is the highest value, for a `greater_than` bound the lowest. Even when the bound never breaks, the watermark tells you how close the system came. A queue bound of 1,000 with `best: 998` is a different story from `best: 12`.

The watermark is for the report only. An always-numeric assertion never calls the discovery hook. It does not fork timelines and does not steer the explorer. If you want the explorer to push a value toward a bound, add a `sometimes` twin that names the direction.

Unlike `assert_always!`, a numeric invariant that is never reached is not a violation. The contract check only inspects its fail count.

## Sometimes: Goals That Ratchet

The four `assert_sometimes_*` macros are guidance. The comparison must hold at least once across all seeds, and every improvement is a discovery.

```rust
// From the dungeon example: push deeper, and arrive with health left.
assert_sometimes_greater_than!(
    i64::from(self.game.status().level),
    0,
    "dungeon level reached"
);
assert_sometimes_greater_than!(i64::from(hp_after), 0, "health remaining");
```

Each evaluation records a pass or a fail and updates two watermarks in the slot:

- The **reporting watermark** is the best left operand, shown as `best:` in the report.
- The **discovery watermark** is the best comparison distance, `left - right`. A `greater_than` goal maximizes it, a `less_than` goal minimizes it.

When the discovery watermark improves, the slot calls the hook with `DiscoveryKind::WatermarkImprovement`. Under exploration that is a fork point. The first improvement happens on the first evaluation, so the explorer branches from level 1, then from level 2, then from level 3. Each better state is a new base for continuations. The watermark only moves one way.

Why track the distance and not the left value? Because the threshold can move during a run. If the right operand grows faster than the left, a larger left value is not progress toward `left > right`. Antithesis made the same choice. The report still prints the plain left value because that is the number a human wants to read.

Two contract rules apply:

1. A slot that was evaluated but never passed is a coverage violation: `numeric assert_sometimes ('...') has 0% success rate (N checks)`.
2. The slot counts for `UntilCoverageStable`. The coverage test `NeverHitNumericGate` pins this: `assert_sometimes_greater_than!(0, 1, "numeric gate")` keeps the run from saturating until the cap.

The `sometimes` numeric macros take no details block. There is no failure to describe: a miss is expected.

## Where the Watermark Lives

The watermarks sit in the slot, in the shared assertion region. Under exploration every forked worker reads the same current best, and a `fetch_max` or `fetch_min` updates it atomically. One worker that reaches level 4 raises the bar for every later worker. The search is collective, not one timeline at a time.

Across seeds without exploration the effect is the same but slower: the table is not reset between seeds, so seed 300 is measured against the best of seeds 1 to 299.

## Use Cases

**A resource bound, with a twin that seeks it.**

```rust
assert_always_less_than!(bytes_in_flight, BUDGET, "memory stays within budget");
// Same value, opposite direction: the explorer now hunts for high memory.
assert_sometimes_greater_than!(bytes_in_flight, BUDGET / 2, "memory pressure reached");
```

The invariant catches the overflow. The guidance drives the explorer to states where overflow is likely.

**A convergence target.**

```rust
assert_sometimes_less_than!(convergence_ms, 500, "cluster converges within 500ms");
```

The watermark ratchets downward. Each faster convergence is a branch point, and the report's `best:` is the fastest convergence any seed achieved.

**A throughput floor across seeds.**

```rust
assert_sometimes_greater_than_or_equal_to!(committed, 1_000, "a seed commits 1,000 transactions");
```

Under chaos most seeds fall short. One seed that gets there clears the coverage violation and tells you the throughput ceiling is reachable at all.

A bound and a direction are two different statements. Write the bound as an invariant. Write the direction as guidance. The two lines together give you correctness and search from the same number.
