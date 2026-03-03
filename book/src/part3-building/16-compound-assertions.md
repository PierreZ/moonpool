# Compound Assertions

<!-- toc -->

Boolean assertions check one condition. Numeric assertions track one value. But the most interesting system states involve multiple things being true at once: a leader is elected **and** all replicas are synced **and** clients are connected. Or we want every distinct configuration to be tested, not just the first one we stumble on. Compound assertions handle both of these cases.

## assert_sometimes_all!: Simultaneous Sub-Goals

Some properties only matter when multiple conditions hold at the same time. A cluster is not truly healthy unless the leader is elected, replicas are caught up, and no partitions are active. Testing each condition individually does not tell you whether the system can achieve them all at once.

`assert_sometimes_all!` takes a message and a list of named boolean sub-goals:

```rust
assert_sometimes_all!("cluster_fully_operational", [
    ("leader_elected", has_leader),
    ("replicas_synced", all_replicas_in_sync),
    ("no_partitions", partition_count == 0),
    ("clients_connected", active_clients > 0),
]);
```

The assertion tracks a **frontier**: the maximum number of sub-goals that have been simultaneously true. It starts at zero. The first time any sub-goal is true, the frontier advances to 1. When two are true at once, it advances to 2. When all four are true simultaneously, the frontier reaches 4 and the assertion is fully satisfied.

Each time the frontier advances, the explorer forks. This creates a progression: the explorer first finds states where one sub-goal is met, then branches from there to find states where two are met, and so on. The exploration naturally follows the path of increasing difficulty.

This is powerful for multi-step objectives. Consider a distributed transaction system. The full operation requires: prepare all participants, get all votes, commit, and acknowledge to the client. As a sometimes-all assertion:

```rust
assert_sometimes_all!("distributed_commit_complete", [
    ("all_prepared", all_participants_prepared),
    ("all_voted_yes", all_votes_received),
    ("committed", transaction_committed),
    ("client_acked", client_received_ack),
]);
```

The explorer works through the stages. First it finds a state where all participants are prepared. It forks from there and pushes for votes. Then it finds committed states. Each stage is a stepping stone to the next.

Without this assertion, the explorer would need to stumble on the complete end-to-end scenario by chance. With it, the explorer gets intermediate checkpoints that guide it through the sequence.

## assert_sometimes_each!: Per-Value Coverage

Sometimes you want to ensure that every **distinct value** of something is explored, not just any one value. If your system has 10 different error codes, you want the simulation to exercise all 10. If a consensus protocol has 5 possible states, you want coverage of each.

`assert_sometimes_each!` creates a separate assertion for each unique combination of identity keys:

```rust
// Ensure every node ID is tested
assert_sometimes_each!("node_tested", [("node_id", node_id)]);

// Ensure every (state, role) combination is explored
assert_sometimes_each!("state_coverage", [
    ("state", state as i64),
    ("role", role as i64),
]);
```

Each unique combination of key values gets its own bucket. The first time a new bucket is discovered, the explorer forks. This equalizes exploration across all discovered values, preventing the simulation from over-concentrating on values it finds easily while neglecting harder-to-reach ones.

The Antithesis team demonstrated this dramatically with The Legend of Zelda. They used `SOMETIMES_EACH` with screen coordinates to ensure the explorer visited all 128 overworld screens and 230 dungeon rooms. Without per-value bucketing, the explorer would revisit the starting area thousands of times while leaving distant rooms unexplored. With it, every room gets roughly equal attention.

For distributed systems, the same pattern applies. If you have a state machine with states {Follower, Candidate, Leader, Observer}, a sometimes-each assertion ensures the simulation exercises each state rather than getting stuck in the most common one:

```rust
assert_sometimes_each!("raft_state_exercised", [
    ("node", node_id),
    ("state", raft_state as i64),
]);
```

## Quality Watermarks

Sometimes discovering a value is not enough. You also want to explore each value under good conditions. `assert_sometimes_each!` supports an optional second list of **quality keys** that track watermarks per bucket:

```rust
assert_sometimes_each!(
    "dungeon_floor_explored",
    [("floor", floor_number)],           // identity: which floor
    [("health", current_health)],         // quality: explore each floor with good health
);
```

Each bucket remembers the best quality values observed. When a bucket is revisited with better quality, the explorer re-forks from that improved state. This prevents the "doomed state" problem described in Antithesis's Castlevania analysis: the explorer reaches an area but in such bad shape that no useful exploration can happen from there. Quality watermarks ensure the representative state for each bucket is the best one found so far.

## State Explosion and Practical Limits

Compound assertions can create a lot of buckets. If you use two identity keys with 100 distinct values each, that is 10,000 potential buckets. Each bucket consumes exploration energy. The explorer bounds energy per assertion to prevent any single assertion from monopolizing the search, but individual assertion effectiveness degrades when buckets proliferate.

Keep identity keys coarse enough to be useful. If you are tracking screen positions in a 256x256 grid, bucket them into 16x16 regions rather than tracking exact pixels. The goal is coverage of **meaningful** distinct states, not exhaustive enumeration of every possible value.

A good rule of thumb: if you would not manually write a separate test for each distinct value, it probably should not be its own bucket.

## Choosing Between the Two

The choice is straightforward:

- **Multiple conditions that must hold simultaneously**: `assert_sometimes_all!`
- **One condition that must hold for each distinct value**: `assert_sometimes_each!`

Use `assert_sometimes_all!` when the challenge is getting several things true at once. Use `assert_sometimes_each!` when the challenge is covering a space of distinct configurations or states.

Both are guidance assertions. Both trigger forks. Both make the explorer smarter about where to search. And both turn a single line of code into something that would otherwise require dozens of hand-crafted test scenarios.
