# Numeric Assertions

<!-- toc -->

Boolean assertions answer yes-or-no questions. But many of the properties we care about in distributed systems are numeric: latency must stay below a threshold, throughput should reach a target, queue depth should not exceed a bound. Numeric assertions let us express these properties directly and give the explorer something extra to work with: a value to optimize.

## Always: Numeric Bounds

The always-numeric macros state bounds that must hold on every check. They work like `assert_always!` but compare two numeric values:

```rust
assert_always_less_than!(queue_depth, max_queue_size, "queue must not overflow");
assert_always_greater_than_or_equal_to!(replica_count, 1, "must have at least one replica");
```

Four comparison operators are available: `assert_always_greater_than!`, `assert_always_greater_than_or_equal_to!`, `assert_always_less_than!`, and `assert_always_less_than_or_equal_to!`. Each takes a value, a threshold, and a message.

When the comparison fails, the behavior is the same as boolean always-assertions: the violation is recorded, an error is logged, and the simulation continues.

But something extra happens behind the scenes. The framework tracks a **watermark** for the value: the most extreme value observed across all checks. For `assert_always_less_than!(x, 100, ...)`, the framework remembers the highest `x` it has seen. Even if `x` never reaches 100, the watermark tells you how close the system came to the boundary.

Why does this matter? Because `assert_always_less_than!(x, 100)` implicitly tells the explorer to **maximize** `x`. The explorer gravitates toward states where `x` is highest, naturally pushing toward the boundary condition. If there is a bug that only manifests when `x` reaches 99, the explorer will find it faster than random exploration would.

This boundary-seeking behavior is automatic. You write a bound, and the framework tries to find states that approach or violate it.

## Sometimes: Numeric Goals

The sometimes-numeric macros state goals that should eventually be achieved. The comparison must hold at least once across all iterations:

```rust
assert_sometimes_greater_than!(throughput, 500, "should achieve >500 ops/sec");
assert_sometimes_less_than!(p99_latency, 100, "p99 should sometimes drop below 100ms");
```

Like their boolean counterparts, these are coverage assertions. If the goal is never achieved after all iterations, the validation reports a coverage violation.

The difference from boolean sometimes is in how they interact with the explorer. **Sometimes-numeric assertions fork on watermark improvement.** The framework tracks the best value observed, and when a new observation beats the previous best, the explorer snapshots and branches from that state.

This creates a ratchet. Suppose you write:

```rust
assert_sometimes_greater_than!(committed_transactions, 1000, "high commit throughput");
```

The first time committed_transactions reaches 50, the explorer branches. Then it finds a timeline where it reaches 200 and branches again. Then 450. Then 800. Each improvement creates a new branch point, and the explorer explores from progressively better states. The watermark only moves in one direction: toward the goal.

## Watermark Mechanics

Every numeric assertion, whether always or sometimes, maintains a watermark in shared memory. The watermark is the best value of the left operand observed so far:

- For assertions with `maximize=true` (greater-than variants, and less-than always-assertions that implicitly drive maximization): the watermark is the highest value seen.
- For assertions with `maximize=false` (less-than sometimes variants): the watermark is the lowest value seen.

The watermark persists across fork boundaries in multiverse mode. When a child timeline improves the watermark, the improvement is visible to subsequent timelines through shared memory. This means the explorer collectively pushes toward the boundary rather than each timeline searching independently.

For sometimes-numeric assertions, a second watermark tracks the value at the last fork point. A new fork only triggers when the value improves **past** the last fork watermark. This prevents the same assertion from triggering unlimited forks for tiny incremental improvements.

## Use Cases

**Resource bounds:**
```rust
assert_always_less_than!(
    memory_usage_mb, max_memory_mb,
    "memory must stay within budget"
);
```
The explorer pushes toward high memory states, testing the system under pressure.

**Convergence targets:**
```rust
assert_sometimes_less_than!(
    convergence_time_ms, 500,
    "cluster should converge within 500ms"
);
```
The explorer ratchets toward fast convergence, branching each time it finds a faster path.

**Throughput validation:**
```rust
assert_always_greater_than_or_equal_to!(
    processed_count, expected_count,
    "must process all submitted requests"
);
```
Catches dropped requests. The explorer seeks states where the gap between processed and expected is smallest (i.e., where the system is closest to dropping something).

The key insight with numeric assertions is that expressing a bound **also** expresses a direction for exploration. You get correctness checking and guided search from the same line of code.
