# Compound Assertions

<!-- toc -->

A boolean assertion checks one condition. A numeric assertion tracks one value. The states that matter most involve several things at once: a leader is elected **and** every replica is caught up **and** no partition is open. Or we want every distinct value covered, not just the first one chaos stumbles on. Two macros handle these cases.

## assert_sometimes_all!: Several Things at Once

`assert_sometimes_all!` takes a message and a list of named booleans.

```rust
assert_sometimes_all!("cluster fully operational", [
    ("leader_elected", has_leader),
    ("replicas_synced", all_replicas_in_sync),
    ("no_partitions", partition_count == 0),
    ("clients_connected", active_clients > 0),
]);
```

The slot tracks a **frontier**: the highest number of propositions that were true at the same time. It starts at 0. The first evaluation with one true proposition moves it to 1, the first with two moves it to 2, and so on. The target is the number of propositions, 4 here. The slot kind is `BooleanSometimesAll`, printed as `frontier` in the report:

```text
  ○  frontier     "cluster fully operational"   1,000 calls  frontier: 3/4  combinations: 5
```

Three contract rules:

1. The status stays `○` (miss) until the frontier reaches the target.
2. The coverage violation reads `assert_sometimes_all!('...') reached frontier 3/4`.
3. `UntilCoverageStable` treats the slot as reached only when the frontier is full.

A frontier advance is a `FrontierAdvance` discovery. That gives the explorer stepping stones. Consider a two-phase commit:

```rust
assert_sometimes_all!("distributed commit complete", [
    ("all_prepared", all_participants_prepared),
    ("all_voted_yes", all_votes_received),
    ("committed", transaction_committed),
    ("client_acked", client_received_ack),
]);
```

The explorer first finds a state where every participant is prepared, forks from there, and pushes for the votes. Then it finds a committed state and forks again. Without the assertion the full sequence must happen by chance in one timeline. With it, each stage is a base for the next.

**Same count, different states.** `leader_elected` alone and `replicas_synced` alone both score 1, but their futures differ. So the slot also keeps a 64-bit bitmap of the truth combinations it has seen. A combination that is new and has at least one true proposition is a `BooleanCombination` discovery, even when the count does not beat the frontier. The bitmap is a bloom filter: a hash collision can lose an optional hint but never touches the frontier or the counts. The report shows the bitmap's population as `combinations:`.

Each evaluation records one pass, so the `calls` figure is the number of times the line ran. The frontier field is a byte: more than 255 propositions is not supported.

## assert_sometimes_each!: Every Distinct Value

Sometimes one pass is not enough. A protocol with five states should visit all five. A cluster with four nodes should see every node take the lead. `assert_sometimes_each!` opens one **bucket** per distinct combination of identity keys.

```rust
// From the maze example: five nested probability gates per lock. Each
// (lock, depth, locks-already-open) tuple is its own bucket, so the report
// shows how deep each lock's chain was ever reached.
assert_sometimes_each!(
    "gate",
    [("lock", lock_id), ("depth", 1i64), ("locks", locks_open)]
);
```

Identity keys are `(&str, integer)` pairs, converted with the same saturating `i64` rule as the numeric macros. A bucket's identity is the FNV-1a hash of the message followed by each key name and value, so `("ab", v)` and `("a", ..), ("b", ..)` cannot collide by concatenation. The first hit of a bucket is a `BucketFirst` discovery. Every later hit adds to the bucket's pass count.

The report groups buckets by site:

```text
━━━ Buckets (23 across 2 sites) ━━━━━━━━━━━━━━━━━━━━━━━━━━
  "gate"                               21 buckets    48,120 hits
  "lock opens"                          2 buckets        19 hits
```

`SimulationReport::bucket_summaries` carries the same numbers as `BucketSiteSummary { msg, buckets_discovered, total_hits }`.

The explorer uses buckets to spread its budget. Antithesis showed this with The Legend of Zelda: a `SOMETIMES_EACH` keyed on screen coordinates made the explorer visit all 128 overworld screens instead of circling the start area. For a state machine the same idea is one line:

```rust
assert_sometimes_each!("raft state exercised", [
    ("node", node_id),
    ("state", raft_state as i64),
]);
```

Moonpool's runner uses it on itself. Every `hint!` decision records `assert_sometimes_each!("hint_struck", [("killed", i64::from(strike == Strike::Killed))])`, so the report shows both outcomes, struck and spared, as two buckets.

## Quality Watermarks

Reaching a bucket is not the same as reaching it in good shape. Antithesis's Castlevania analysis calls the bad case a **doomed state**: the explorer gets to the boss room with one hit point and nothing useful can follow. An optional second list of **quality keys** fixes that.

```rust
// From the dungeon example: every floor, and every floor with health left.
assert_sometimes_each!(
    "stairs with key",
    [("floor", i64::from(level))],
    [("health", i64::from(hp_bucket))]
);
```

Each bucket remembers its best quality score. A hit with a better score is a `BucketQuality` discovery, and the explorer re-forks from the healthier state. Up to four quality keys are packed into one 64-bit score. The first key is the most significant. Each key keeps only its low 16 bits, so scale values into `0..=65535` before passing them, and keep "higher is better" as the convention.

Quality values are not part of the bucket's identity. Health 3 and health 9 on floor 2 are the same bucket with two scores.

## Budgets and Limits

The bucket table is fixed. The constants are a contract that changes the saturation history of every campaign, so treat them as a wire format.

| Limit | Value | Where |
|-------|-------|-------|
| Buckets per run | 256 | `MAX_EACH_BUCKETS` |
| Identity keys per site | 6 | `MAX_EACH_KEYS`, extra keys are ignored |
| Quality keys per site | 4 | extra keys are ignored |
| Message bytes stored per bucket | 32 | longer messages are truncated for identity |
| Propositions per `sometimes_all` | 255 | the frontier is a `u8` |

Overflow is loud. An observation that finds the table full is counted, and the run fails with `each-bucket table dropped N sometimes_each observations`. A bucket reached by a different message or key set with the same hash is flagged, and the run fails with `bucket [..] collides with another key set`.

256 buckets go fast. Two identity keys with 100 values each would need 10,000. Keep keys coarse: bucket a 256x256 grid into 16x16 regions, bucket hit points into four bands as the dungeon example does with `hp_bucket`. A good test: if you would not write a separate test case for each value, it should not be its own bucket.

## Choosing Between the Two

- Several conditions that must hold **at the same time**: `assert_sometimes_all!`.
- One condition that must hold **for every value**: `assert_sometimes_each!`.

`assert_sometimes_all!` has a contract: the run reports a gap until every proposition was true together. `assert_sometimes_each!` has none, because the table cannot list a bucket it never saw. Its coverage statement is the bucket list in the report. Both are guidance assertions. Both fork. Both turn one line into coverage that would otherwise take dozens of hand-written scenarios.
