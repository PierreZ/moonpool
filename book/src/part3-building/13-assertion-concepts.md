# Invariants vs Discovery vs Guidance

<!-- toc -->

Not every assertion asks the same question. Some say "this must always be true". Some say "this must happen at least once". A few say "try to make this number better". The runner, the stop condition and the explorer treat the three kinds differently, so the kind you pick decides what a violation means.

## Invariants: What Must Always Hold

An **invariant** must be true every time it is evaluated. One failure is a bug.

```rust
// From the leader-election test: one leader per term, or split-brain.
assert_always!(
    prior == &leader,
    format!("split-brain at term {term}: {prior} vs {leader}")
);
```

Six macros are invariants: `assert_always!`, `assert_always_or_unreachable!`, `assert_unreachable!` and the four `assert_always_*` numeric comparisons. In Antithesis's own Pangolin database about 90% of assertions are of this kind, and that ratio holds in moonpool's examples too. Most of what we know about a system is "this property holds".

Invariants do not talk to the explorer. A failure records a violation and marks the seed failed. A pass records nothing interesting. The numeric forms keep a watermark for the report, but a new best value is **not** a discovery and does not fork a timeline.

There is one trap. `assert_always!` fails when it is **never reached** across the whole run. An invariant that guards a recovery path your simulation never enters tells you nothing, so the contract check flags it: `assert_always!('...') was never reached`. For a path that may legitimately stay cold under some configurations, use `assert_always_or_unreachable!`.

## Discovery: What Must Happen at Least Once

A **discovery** assertion must pass at least once across all seeds. It may fail a thousand times before that.

```rust
// From the coverage test: fires in roughly one seed out of five.
let rare = moonpool_sim::sim_random_range(0u32..5) == 0;
assert_sometimes!(rare, "rare gate");

// From the runner's own hint code: a path, no condition.
assert_reachable!("hint: a process rebooted at its own hint");
```

Discovery assertions prove coverage. Did a failover happen? Did the retry path run? Without them a simulation can run a thousand seeds down the happy path and report zero failures. Green, and meaningless.

They have three effects:

1. **Contract.** A `sometimes` that was evaluated but never passed, or a `reachable` never reached, is a coverage violation in the report.
2. **Stop condition.** Under `UntilCoverageStable`, the run cannot saturate until every observed discovery assertion has passed. A rare gate keeps the campaign alive.
3. **Fork point.** The first pass flips an atomic latch in the slot and calls the discovery hook with `DiscoveryKind::SometimesPass`. Under exploration, the explorer records the RNG call count and forks continuations from that moment.

The third effect is why discovery assertions have superpowers. Suppose a bug needs a failover (1 in 1,000) and then a specific timing during recovery (1 in 1,000). Random seeds need about a million runs. With `assert_sometimes!(failed_over, ...)`, the explorer finds the failover in about a thousand runs, keeps its replay recipe, and tries a thousand continuations from there. Multiplicative odds become additive. [Multiverse Exploration](../part5-building-on-top/01-exploration.md) covers the mechanics.

## Guidance: What to Push Toward

A **guidance** assertion names a direction. It is not satisfied once and done. Every improvement is a new discovery.

```rust
// From the dungeon example: deeper is better, and the explorer knows it.
assert_sometimes_greater_than!(i64::from(level), 0, "dungeon level reached");

// Every floor, and every floor reached with good health.
assert_sometimes_each!(
    "stairs with key",
    [("floor", i64::from(level))],
    [("health", i64::from(hp_bucket))]
);
```

Three families guide:

- The four `assert_sometimes_*` numeric comparisons track a **watermark**. A better comparison distance (`left - right`) is a `WatermarkImprovement` discovery.
- `assert_sometimes_all!` tracks a **frontier**: the most propositions true at the same time. A higher count is a `FrontierAdvance`; a new partial combination at the same count is a `BooleanCombination`.
- `assert_sometimes_each!` tracks **buckets**: one per distinct identity-key tuple. A new bucket is a `BucketFirst`; a better quality score in a known bucket is a `BucketQuality`.

Each discovery is latched with an atomic compare-and-swap in the shared table, so across any number of forked workers it fires exactly once. The explorer ranks them: watermark and frontier improvements are **progress**, first passes and new buckets are **coverage**. That ratchet drives the search toward boundaries, which is where bugs sit.

## What Each Kind Does

| Kind | Macros | Seed fails when | Coverage violation when | `UntilCoverageStable` waits for it | Explorer forks on |
|------|--------|-----------------|-------------------------|------------------------------------|-------------------|
| Invariant | `assert_always!`, `assert_always_or_unreachable!`, `assert_unreachable!`, `assert_always_*` numeric | condition false, or unreachable reached | never | no | nothing |
| Discovery | `assert_sometimes!`, `assert_reachable!` | never | never passed | yes | first pass |
| Guidance | `assert_sometimes_*` numeric, `assert_sometimes_all!`, `assert_sometimes_each!` | never | numeric never passed, frontier never full | numeric and `sometimes_all`: yes. `sometimes_each`: no | every improvement, new combination or new bucket |

Two cells deserve a second look. `assert_sometimes_each!` has no contract: a bucket you never hit cannot be listed, because the table does not know it exists. The report lists the buckets found, and that is the coverage statement. And the `must_hit` flag that makes a cold `assert_always!` fail is set only on `assert_always!` itself, not on its `_or_unreachable` twin.

## Picking the Kind

Ask one question per line:

- Must this never be false? **Invariant.** Add `_or_unreachable` if the path may stay cold.
- Must this happen at least once? **Discovery.** Use `assert_reachable!` when there is no condition.
- Is there a number, a set of flags or a space of values to push through? **Guidance.**

A good workload has many invariants, a handful of discoveries on its error and recovery paths, and one or two guidance assertions that give the explorer a direction. The next three chapters show each family in detail.
