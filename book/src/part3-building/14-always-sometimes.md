# Always and Sometimes

<!-- toc -->

The boolean assertion macros are the foundation of everything else in moonpool's assertion system. There are five of them. Each takes a message string that identifies the assertion across iterations and across forked timelines.

## assert_always!

The workhorse. This states a property that must hold every time it is evaluated.

```rust
assert_always!(
    replica_count >= min_replicas,
    "replica count must meet minimum"
);
```

If the condition is false, moonpool records the violation at ERROR level with the current seed and increments the failure counter. It does **not** panic. The simulation continues, and subsequent assertions in the same iteration can still fire and record their own results.

There is one behavior that surprises people at first: `assert_always!` also fails if it is **never reached**. The reasoning is that an untested invariant provides false confidence. If you write an assertion guarding your recovery path but the simulation never exercises recovery, the post-run validation will flag it. This forces you to either fix your simulation to reach that path or use a different assertion type.

## assert_always_or_unreachable!

Same semantics as `assert_always!`, except it passes silently when the code path is never executed.

```rust
assert_always_or_unreachable!(
    balance >= 0,
    "balance must be non-negative after withdrawal"
);
```

Use this for properties that guard optional or conditional code paths. If a particular fault injection scenario never triggers a withdrawal, this assertion will not flag a coverage gap. But if the path **is** reached and the balance goes negative, that is a violation.

The distinction between `assert_always!` and `assert_always_or_unreachable!` prevents a common trap: littering your code with always-assertions, then getting flooded with "never reached" violations because your simulation configuration does not exercise every path. Reserve `assert_always!` for the paths you **must** test. Use `assert_always_or_unreachable!` for the rest.

## assert_sometimes!

This states a property that should be true at least once across all iterations. It does not need to hold every time.

```rust
assert_sometimes!(
    connections_dropped > 0,
    "chaos should sometimes drop connections"
);
```

If the condition is never true after all iterations complete, the post-run validation reports a coverage violation. But unlike always-violations, coverage violations are statistical. They become meaningful only after enough iterations.

The real power of `assert_sometimes!` shows up in multiverse mode. When the condition fires true for the first time in a timeline, the explorer snapshots that moment and branches. New timelines start from that interesting state. This is what transforms sometimes-assertions from passive coverage checks into active exploration amplifiers.

Think of it this way: `assert_sometimes!` is how you tell the explorer "this state is worth investigating." The explorer does the rest.

Here is a practical pattern. You want to verify that your system handles leader re-election after a crash:

```rust
// In your workload
assert_sometimes!(
    leader_changed_after_crash,
    "leader should change after crash"
);
```

Without multiverse exploration, this assertion just checks that your simulation exercises the re-election path. With exploration enabled, it creates a checkpoint at the moment re-election succeeds. The explorer then branches from that checkpoint, increasing the chance of finding bugs in the post-election state.

## assert_reachable!

A simplified form of sometimes-assertion for code paths that should be hit at least once. No condition needed.

```rust
fn handle_timeout(&mut self) {
    assert_reachable!("timeout handler executed");
    // ... handle the timeout
}
```

This is equivalent to `assert_sometimes!(true, "...")` but reads more clearly when you just want to confirm a path is exercised. Like `assert_sometimes!`, it triggers a fork on first reach in multiverse mode.

## assert_unreachable!

The inverse: marks a code path that should never execute.

```rust
fn handle_message(&mut self, msg: Message) -> Result<(), Error> {
    match msg.kind {
        MessageKind::Request => { /* ... */ }
        MessageKind::Response => { /* ... */ }
        MessageKind::Unknown => {
            assert_unreachable!("received unknown message kind");
            return Err(Error::UnknownMessage);
        }
    }
    Ok(())
}
```

If this code path is reached, moonpool records a violation (equivalent to an always-violation). Unlike `assert_always!`, there is no condition to check. Being reached at all is the violation.

Note that the code after the assertion **still executes**. The assertion records the problem but does not prevent the error path from running. This lets the simulation discover what happens when "impossible" conditions occur.

## Choosing the Right One

The decision tree is straightforward:

- **Must hold every time, must be tested**: `assert_always!`
- **Must hold every time, might not be reached**: `assert_always_or_unreachable!`
- **Must happen at least once**: `assert_sometimes!`
- **Path must be exercised**: `assert_reachable!`
- **Path must never execute**: `assert_unreachable!`

In practice, the distribution follows a pattern similar to what Antithesis found with their Pangolin database: roughly 90% always-type, with sometimes/reachable filling in the coverage gaps. That ratio makes sense because most of what we want to say about a system is "this must hold," not "this should eventually happen."

Start with always-assertions on your core invariants. Add sometimes-assertions on the interesting error paths and recovery scenarios. The explorer will use those sometimes-assertions to find the bugs hiding behind the invariants.
