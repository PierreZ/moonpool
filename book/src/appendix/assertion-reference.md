# Assertion Reference

<!-- toc -->

Moonpool provides **15 assertion macros** for testing distributed system properties. All macros follow the Antithesis principle: **assertions never crash your program**. Violations are recorded in shared memory and reported after the simulation completes, allowing the system to continue running and discover cascading failures.

Every macro is tracked in shared memory via moonpool-explorer, enabling cross-process visibility across forked exploration timelines.

## Boolean Assertions

These macros test boolean conditions. Always-type assertions record violations but do not panic. Sometimes-type assertions guide exploration by triggering forks on discovery.

| Macro | Category | Description | Panics? | Forks in exploration? |
|-------|----------|-------------|---------|----------------------|
| `assert_always!` | Always | Condition must be true every time it is evaluated | No | No |
| `assert_always_or_unreachable!` | Always | Condition must be true when reached, but the code path need not be reached | No | No |
| `assert_sometimes!` | Sometimes | Condition should be true at least once across all iterations | No | Yes, on first success |
| `assert_reachable!` | Reachable | Code path should be reached at least once | No | Yes, on first reach |
| `assert_unreachable!` | Unreachable | Code path should never be reached | No | No |

### `assert_always!(condition, message)`

Records a violation if `condition` is false. The simulation continues; violations are collected and reported at the end. Validated by `validate_assertion_contracts()` which flags the assertion if `fail_count > 0`, or if `must_hit` and the assertion was never reached.

```rust
assert_always!(
    granted_count <= 1,
    "lock never granted to two nodes simultaneously"
);
```

### `assert_always_or_unreachable!(condition, message)`

Like `assert_always!`, but does **not** require the code path to be reached. If the code is never executed, the assertion passes silently. Useful for guarding optional error-handling paths.

```rust
assert_always_or_unreachable!(
    retry_count < max_retries,
    "retry count within bounds when retry path taken"
);
```

### `assert_sometimes!(condition, message)`

Records pass/fail statistics. On the **first** time the condition is `true`, triggers a fork in exploration mode to explore alternate timelines from that point. Validated by checking that `pass_count > 0` after enough iterations.

```rust
assert_sometimes!(
    saw_leader_election,
    "leader election triggered at least once"
);
```

### `assert_reachable!(message)`

Marks a code path as "should be reached." Always passes `true` as the condition. On first reach, triggers a fork. Validated by checking that `pass_count > 0`.

```rust
if connection_failed {
    assert_reachable!("retry path exercised");
    retry().await;
}
```

### `assert_unreachable!(message)`

Marks a code path that should **never** execute. If reached, records a violation (but does not panic). Validated by checking that `pass_count == 0`.

```rust
match state {
    State::Valid => { /* ok */ }
    State::Invalid => {
        assert_unreachable!("invalid state should never occur");
    }
}
```

## Numeric Assertions

These macros compare a value against a threshold. Always-type numeric assertions record violations on failure. Sometimes-type numeric assertions track **watermarks** (best observed value) and trigger forks when the watermark improves.

All values are cast to `i64` internally.

| Macro | Category | Comparison | Panics? | Forks in exploration? |
|-------|----------|------------|---------|----------------------|
| `assert_always_greater_than!` | NumericAlways | `val > threshold` | No | No |
| `assert_always_greater_than_or_equal_to!` | NumericAlways | `val >= threshold` | No | No |
| `assert_always_less_than!` | NumericAlways | `val < threshold` | No | No |
| `assert_always_less_than_or_equal_to!` | NumericAlways | `val <= threshold` | No | No |
| `assert_sometimes_greater_than!` | NumericSometimes | `val > threshold` | No | Yes, on watermark improvement |
| `assert_sometimes_greater_than_or_equal_to!` | NumericSometimes | `val >= threshold` | No | Yes, on watermark improvement |
| `assert_sometimes_less_than!` | NumericSometimes | `val < threshold` | No | Yes, on watermark improvement |
| `assert_sometimes_less_than_or_equal_to!` | NumericSometimes | `val <= threshold` | No | Yes, on watermark improvement |

### Always numeric example

```rust
assert_always_greater_than!(
    queue.len(),
    0,
    "queue never empty during processing"
);

assert_always_less_than_or_equal_to!(
    latency_ms,
    timeout_ms,
    "response latency within timeout"
);
```

### Sometimes numeric example

```rust
assert_sometimes_greater_than!(
    concurrent_connections,
    5,
    "achieved high concurrency"
);

assert_sometimes_less_than!(
    retry_delay_ms,
    100,
    "fast retry path exercised"
);
```

**Watermark tracking**: For `sometimes` numeric assertions, the explorer tracks the best value observed so far. When a new evaluation **improves** the watermark (higher for `gt`/`ge`, lower for `lt`/`le`), a fork is triggered to explore timelines that might push the metric even further.

## Compound Assertions

These macros track multi-dimensional properties across multiple conditions or identity keys.

| Macro | Category | Description | Panics? | Forks in exploration? |
|-------|----------|-------------|---------|----------------------|
| `assert_sometimes_all!` | BooleanSometimesAll | All named booleans should sometimes be true simultaneously | No | Yes, on frontier advance |
| `assert_sometimes_each!` | EachBucket | Per-identity bucketed assertion with optional quality watermarks | No | Yes, on new bucket or quality improvement |

### `assert_sometimes_all!(message, [(name, bool), ...])`

Tracks a **frontier**: the maximum number of conditions that have been simultaneously true. When the frontier advances (more conditions true at once than ever before), a fork is triggered.

```rust
assert_sometimes_all!("all_nodes_healthy", [
    ("node_a", node_a_healthy),
    ("node_b", node_b_healthy),
    ("node_c", node_c_healthy),
]);
```

If previously at most 2 of the 3 conditions were true at once, and now all 3 are true, the frontier advances from 2 to 3 and a fork is triggered.

### `assert_sometimes_each!(message, [(key, value), ...])` / `assert_sometimes_each!(message, [(key, value), ...], [(quality_key, quality_value), ...])`

Each unique combination of identity keys gets its own **bucket**. On first discovery of a new bucket, a fork is triggered. If quality keys are provided, the explorer also tracks quality watermarks per bucket and re-forks when quality improves.

```rust
// Identity keys only -- fork on first discovery of each (lock, depth) combo
assert_sometimes_each!("gate", [("lock", lock_id), ("depth", depth)]);

// With quality watermarks -- also fork when health improves for a known bucket
assert_sometimes_each!("descended", [("to_floor", floor)], [("health", hp)]);
```

## Validation: `validate_assertion_contracts()`

After the simulation completes, `validate_assertion_contracts()` reads all assertion slots from shared memory and checks each one against its kind-specific contract. It returns two vectors:

### Always violations (definite bugs)

These indicate real bugs and are safe to check regardless of iteration count.

| Kind | Violation condition |
|------|-------------------|
| `Always` | `fail_count > 0` (condition was false at least once) |
| `Always` | `must_hit && total == 0` (assertion was never reached) |
| `AlwaysOrUnreachable` | `fail_count > 0` (condition was false when reached) |
| `Unreachable` | `pass_count > 0` (code path was reached) |
| `NumericAlways` | `fail_count > 0` (comparison failed at least once) |

### Coverage violations (statistical)

These are only meaningful with enough iterations for statistical coverage. A single iteration may not trigger every path.

| Kind | Violation condition |
|------|-------------------|
| `Sometimes` | `total > 0 && pass_count == 0` (condition was never true) |
| `Reachable` | `pass_count == 0` (code path was never reached) |
| `NumericSometimes` | `total > 0 && pass_count == 0` (comparison never succeeded) |
| `BooleanSometimesAll` | No simple violation (frontier tracking is the guidance mechanism) |

## Related Functions

| Function | Purpose |
|----------|---------|
| `record_always_violation()` | Increment the thread-local violation counter (called by always-type macros) |
| `reset_always_violations()` | Reset the violation counter (called at the start of each iteration) |
| `has_always_violations() -> bool` | Check if any always-type violation occurred this iteration |
| `get_assertion_results() -> HashMap<String, AssertionStats>` | Read all assertion statistics from shared memory |
| `reset_assertion_results()` | Zero the shared memory assertion table (between iterations) |
| `skip_next_assertion_reset()` | Prevent the next reset (used by multi-seed exploration) |
| `panic_on_assertion_violations(report)` | Panic if the report contains any assertion violations |
