# Invariants vs Discovery vs Guidance

<!-- toc -->

Not all assertions ask the same kind of question. Some say "this must always be true." Others say "this should happen at least once." And a few say "try to make this better." These are fundamentally different kinds of statements, and the simulation framework treats them differently.

Moonpool organizes assertions into three categories based on their purpose. Getting the category right matters because it determines how the assertion interacts with the runner, how it interacts with the explorer, and what a violation actually means.

## Invariants: What Must Always Hold

**Invariant assertions** state properties that must be true every single time they are checked. If they fail even once, there is a bug.

```rust
assert_always!(committed_count <= total_count, "commits cannot exceed total");
```

This is the assertion you reach for most often. In Antithesis's own Pangolin database, about 90% of assertions are always-type. That ratio matches what we see in practice: the vast majority of what we want to say about a system is "this property holds."

Invariant assertions do **not** interact with the explorer. They do not trigger forks or snapshots. Their job is purely to catch violations. When one fails, the runner records the violation and continues. The value is in the recording, not in the stopping.

There is an important subtlety: `assert_always!` also fails if it is **never reached**. An assertion that is never evaluated gives false confidence. If you have an assertion guarding a recovery path but your simulation never triggers recovery, the assertion tells you nothing. Moonpool flags unreached always-assertions as violations so you know your coverage has gaps.

For optional code paths where non-reachability is acceptable, use `assert_always_or_unreachable!` instead. It validates the property when reached but passes silently if the code path is never exercised.

## Discovery: What Must Happen Eventually

**Discovery assertions** state properties that must occur at least once across all simulation iterations. They do not need to hold every time, but they must fire true at some point.

```rust
assert_sometimes!(leader_elected, "a leader should eventually be elected");
assert_reachable!("recovery path exercised");
```

Where invariants validate, discovery assertions prove coverage. They answer questions like: Did our simulation actually trigger a failover? Did a leader election happen? Did the retry path execute? Without discovery assertions, a simulation could run thousands of iterations exercising only the happy path and report zero failures. Everything looks green, but nothing interesting was tested.

Discovery assertions become **exploration amplifiers** in multiverse mode. When `assert_sometimes!` fires true for the first time, the explorer snapshots the simulation state and branches from that point. Why? Because reaching that state was hard, and there are likely more interesting states reachable from it.

Consider a bug that requires a failover (probability 1/1000) followed by a specific timing condition during recovery (probability 1/1000). Without exploration amplification, finding this bug requires roughly 1,000,000 random iterations. With a sometimes-assertion on the failover that triggers branching, the explorer takes a shortcut: find the failover once in ~1000 iterations, snapshot it, then find the timing condition in ~1000 more iterations from that checkpoint. The effective probability drops from multiplicative to additive.

This is why discovery assertions have "superpowers" in moonpool. They are not just coverage markers. They are active participants in the search for bugs.

## Guidance: What to Optimize Toward

**Guidance assertions** steer the explorer toward interesting regions of the state space. They express goals that the system should try to achieve, and the explorer actively works to satisfy them.

```rust
assert_sometimes_greater_than!(throughput, 1000, "should sometimes achieve high throughput");

assert_sometimes_all!("full_cluster_ready", [
    ("leader_elected", has_leader),
    ("all_replicas_synced", replicas_in_sync),
    ("clients_connected", clients_ready),
]);
```

Numeric guidance assertions track watermarks. The system remembers the best value it has observed and forks when it discovers a better one. This creates a ratchet effect: the explorer steadily pushes toward boundary conditions where bugs tend to hide.

Compound guidance assertions track frontiers. `assert_sometimes_all!` counts how many sub-goals are simultaneously true and forks when that count increases. The explorer is driven to satisfy more sub-goals at once, which naturally leads it toward complex system states that are hard to reach by random exploration alone.

## Why the Taxonomy Matters

The three categories are not just organizational labels. They determine what the framework does with the assertion:

| Category | Runner behavior | Explorer behavior | Violation meaning |
|----------|----------------|-------------------|-------------------|
| **Invariant** | Flags failure | Nothing | Definite bug |
| **Discovery** | Checks coverage | Forks on first success | Insufficient testing |
| **Guidance** | Reports progress | Forks on improvement | Exploration target |

When writing an assertion, ask yourself: Am I checking a property that must never be violated? Am I verifying that something interesting happened? Or am I telling the explorer where to look? The answer determines which macro to use, and using the right one makes the entire framework more effective.
