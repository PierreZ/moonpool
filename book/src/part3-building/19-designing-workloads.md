# Designing Workloads That Find Bugs

<!-- toc -->

A workload that exercises only the happy path finds nothing. The earlier chapters covered **how** to write a workload, wire assertions and register invariants. This chapter is about **what** to put inside: which operations to generate, what to measure, and why one wide workload beats ten narrow ones.

## Strategy vs Tactics

Will Wilson's framework for simulation workloads has two independent dimensions.

**Strategy** is what you measure and optimise: code coverage, grid cells visited, time the system stays alive. Strategy decides whether a run was productive.

**Tactics** is how you generate inputs: uniform random, weighted toward writes, biased toward boundary values. Tactics is the raw material.

They are independent. Brilliant tactics with crude strategy works. Coverage-guided strategy with pure random tactics works. You need at least one to be good. Moonpool's strategy is built in: `UntilCoverageStable` watches real code coverage and the assertion table, and [exploration](../part5-building-on-top/01-exploration.md) forks at new assertion outcomes. Tactics is yours.

But "pure random" has a trap.

**The white noise paradox.** Random inputs are maximally random at the micro level and minimally random at the macro level. Drop each packet with 5% probability and a sustained 10-second partition has probability 0.05 to the power of every packet crossing the link in those seconds. Zero, in practice. Random drops average out to a slightly degraded, fundamentally healthy network. The same holds for random user inputs: random button presses average out to no sustained action. The system never reaches the deep states where bugs hide.

This is why Moonpool's faults are **correlated**: a partition that exists at t=0 is almost certainly still there at t=1, and a new one starting is rare. The same reasoning shapes the workload.

## The Operation Alphabet

The operations a workload generates fall into three classes.

**Normal operations** are production-like traffic. Reads, writes, queries. Match the real ratio: 80% reads and 20% writes if that is what users do.

**Adversarial inputs** are what users actually send. Empty strings. Boundary values (0, -1, `u64::MAX`). Maximum-length fields. Keys that collide. A workload without them tests a polite fiction.

**Nemesis operations** deliberately break things. Kill a process mid-transaction. Trigger compaction under peak writes. Send conflicting writes to one key from several clients at once.

Normal operations test the happy path. Adversarial inputs test the validation path. Nemesis operations test the recovery path, where most bugs live.

```rust
match ctx.random().random_range(0..100) {
    0..60 => self.do_normal_operation(ctx).await?,
    60..80 => self.do_adversarial_operation(ctx).await?,
    _ => self.do_nemesis_operation(ctx).await?,
}
```

Tune the weights from what the `assert_sometimes!` rows tell you.

## Swarm the Alphabet

A complete alphabet has a hidden failure mode. Picking uniformly over the full set on every step keeps every operation slightly active, and they crowd each other out. A bounded queue with a 50/50 push/pop mix is a one-dimensional random walk. By the Hoeffding bound it takes tens of millions of steps to drift far enough to fill. The overflow bug is real, reachable, and never hit, because `pop` undoes what `push` makes. This is **passive suppression**, the effect [swarm testing](./28-swarm-testing.md) defeats for fault families, now inside the workload.

The fix is the same. Enable a random **subset** of the alphabet per seed and turn the rest fully off. Disable `pop` for some seeds and the queue fills in a handful of steps.

```rust
SimulationBuilder::new()
    .swarm_operations()
    .workload_factory(|| Box::new(QueueWorkload::new()))
```

`moonpool_sim::swarm_op_enabled(op_id)` tells the workload whether operation `op_id` is in this seed's subset. Without `.swarm_operations()` every operation reports enabled, so a default run uses the full alphabet and nothing changes. The dungeon example in `crates/moonpool-sim-examples/src/dungeon.rs` is the reference:

```rust
/// Each action is independently enabled about half the time this seed. A seed
/// that disables everything falls back to the full alphabet so the workload
/// always has something to do.
fn swarm_enabled_actions() -> Vec<u8> {
    let enabled: Vec<u8> = (0..NUM_ACTIONS)
        .filter(|&i| moonpool_sim::swarm_op_enabled(i))
        .collect();
    if enabled.is_empty() { (0..NUM_ACTIONS).collect() } else { enabled }
}

/// One draw per step, folded into the enabled subset.
fn pick_action_index(&mut self) -> u8 {
    let raw = moonpool_sim::sim_random::<u64>();
    let span = u64::try_from(self.enabled_actions.len()).expect("alphabet size fits in u64");
    self.enabled_actions[usize::try_from(raw % span).expect("index fits")]
}
```

The remap matters. We draw **once** and fold into the subset. A loop that resamples until it lands on an enabled operation consumes a variable number of draws per step, and exploration keys its replay on the RNG call count. Keep the per-step draw count fixed and replay stays exact. When the full alphabet is enabled, `enabled[raw % n] == raw % n`, so a non-swarm run is byte-identical to one with no swarm code at all.

The mask is 256 bits, one per `u8` operation id, drawn in the first four counted draws of every seed before any attrition or outage regime. `swarm_op_enabled` only reads a bit, so asking consumes nothing and the answer never depends on how often you ask. The two swarm axes compose: a seed picks a fault subset **and** an operation subset.

The payoff is an assertion only the swarm can reach. Emit it on the path where it holds, so it never records a check on seeds where the subset makes it impossible:

```rust
// Reachable only when every movement operation was masked off this seed.
if !enabled.iter().any(|&i| i < NUM_MOVE_OPS) {
    moonpool_sim::assert_sometimes!(true, "movement suppressed by swarm");
}
```

`crates/moonpool-sim/tests/swarm_op_alphabet.rs` shows the assertion firing under `.swarm_operations()` and staying unreachable without it.

## One Workload, Many Behaviours

It is tempting to write one workload per scenario: a read-heavy client, a write storm, a scanner, a crash-at-commit driver. Resist it. Every extra workload is a separate operation mix that must be scheduled, named and maintained, and the scenarios never combine. The write storm never meets the scanner on the same seed.

Prefer **one workload with a wide alphabet** and let the per-seed draws produce the scenarios:

- `swarm_operations()` picks which operations exist this seed. The read-heavy client is the seed where writes are masked off. The write storm is the seed where reads are.
- `WorkloadCount::Random(1..6)` picks how many instances run, so contention varies.
- `ChaosMode::Swarm` picks which fault families are on, so the same alphabet meets partitions on one seed and disk stalls on the next.
- Inside the shipped code, `buggify_named!` sites and `set_activation` give a seed its own set of active scenarios, which [Buggify](./08-buggify.md) covers.

One alphabet, one model, one set of assertions, thousands of behaviours. The campaign covers combinations no hand-written scenario list would contain, and a failing seed is still a single number to replay.

## Invariant Patterns

Generating interesting operations is half the problem. Knowing whether the system **behaved** is the other half. Four patterns hold up.

**Reference models** keep an in-memory expected state and compare after every operation, as the [workload chapter](./03-writing-workload.md) did. Use `BTreeMap`, never `HashMap`, so iteration order replays.

**Conservation laws** check quantities that must stay constant. Total record count across shards. Messages sent minus messages received. Committed values that must agree across replicas. [Events and Invariants](./17-events-and-invariants.md) builds one.

**Structural integrity** walks a data structure after chaos. No cycles in a list. Balance at every B-tree level. Parent pointers that agree with children. These catch corruption a reference model cannot see.

**Operation logging** resolves commit uncertainty. A write that failed with a network error may or may not have committed. Log the intent beside the mutation, then reconcile the log against the state after recovery. Every simulation workload talks through an unreliable network, so every one needs this.

## Assertions as Memos

Lawrie Green's view: an assertion serves two readers at once.

It informs the **computer**. `assert_sometimes!` tells the explorer "this state is interesting, branch here". `assert_always!` tells the runner "if this fails, stop and report".

It documents the **developer's mental model**. `assert_always!(balance >= 0, "balance never negative")` records a belief. When it fails on correct code, the belief was wrong, and that is itself a finding.

So assertions belong everywhere a developer has an opinion: the error path, the recovery path, the "this cannot happen" path that will happen in simulation.

## Generate Sufficient Workload

One client sending sequential requests never triggers the races where distributed bugs hide. The system needs concurrency:

```rust
SimulationBuilder::new()
    .processes(3, || Box::new(KvServer))
    .workloads(WorkloadCount::Fixed(5), |_| Box::new(KvWorkload::new(200)))
    .run()?
```

Five clients, 200 operations each, three servers, chaos on. The interleavings between concurrent operations across faulting servers are where bugs live.

There is a deeper principle. Hand-written test cases scale **multiplicatively** with system complexity: a 300-line feature can need 10,000 lines of tests for combinatorial coverage. A generator scales **linearly**. One new variant in the operation enum meets every other operation, every fault family and every timing on its own:

```rust
enum Operation {
    Get { key: String },
    Set { key: String, value: Vec<u8> },
    Delete { key: String },
    Scan { start: String, end: String },
    // One line. The simulator pairs it with everything above, every fault
    // family, and every interleaving, across every seed.
    CompareAndSwap { key: String, expected: Vec<u8>, new: Vec<u8> },
}
```

That is the real payoff of simulation testing. Not that each test is better, but that the **cost of coverage** changes from exponential to linear.
