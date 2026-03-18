# Designing Workloads That Find Bugs

<!-- toc -->

A workload that exercises only the happy path is a workload that finds nothing. The previous chapters covered **how** to write a workload, wire up assertions, and register invariants. This chapter is about **what** to put inside them. What operations to generate, what to measure, and how to think about the relationship between input design and bug discovery.

## Strategy vs Tactics

Will Wilson offers a useful framework for thinking about simulation workloads. There are two independent dimensions: **strategy** and **tactics**.

**Strategy** is what you measure and optimize. Code coverage, grid cells visited, time the system stays alive, request throughput. Strategy determines how the framework judges whether a run was productive.

**Tactics** is how you generate inputs. Uniform random? Weighted toward writes? Biased toward boundary values? Tactics determines the raw material the simulator works with.

These dimensions are **independent**. A workload with brilliant tactics (generating exactly the right fault sequences) can get by with crude strategy (just measuring crashes). A workload with sophisticated strategy (coverage-guided exploration) can get by with simple tactics (pure random inputs). You do not need both to be perfect. You need at least one to be good.

But there is a trap in "pure random" that deserves attention.

**The white noise paradox**: random inputs are maximally random at the micro level but minimally random at the macro level. Consider random per-packet network faults. Each packet has a 5% chance of being dropped. Sounds adversarial. But the probability of a sustained 10-second partition is 0.05 raised to the power of however many packets cross that link in 10 seconds. Essentially zero. Random drops average out to a slightly degraded but fundamentally healthy network. No sustained partition ever forms.

The same applies to random user inputs. Random button presses in a video game average out to no sustained action. Random API calls average out to no sustained workflow. The system never enters the deep states where bugs hide.

This is why moonpool uses **correlated fault injection** rather than random per-event drops. "The probability that there's a network partition happening at time t=1 is highly dependent on whether there's a network partition happening at time t=0." A new partition starting is rare. A partition continuing is nearly certain. Correlated distributions produce the sustained fault patterns that expose real bugs.

## The Operation Alphabet

From practical experience building workloads, we have found that the operations a workload generates fall into three categories. We call these the **Operation Alphabet**.

**Normal operations** are production-like traffic. Reads, writes, queries, transactions. The distribution should match what real users do. If your system handles 80% reads and 20% writes, your workload should approximate that ratio.

**Adversarial inputs** are what users actually send, whether you planned for it or not. Empty strings. Boundary values (0, -1, `u64::MAX`). Unicode edge cases. Maximum-length fields. Keys that collide in your hash function. A workload that skips adversarial inputs is testing a polite fiction.

**Nemesis operations** deliberately break things. Kill a process mid-transaction. Trigger a compaction during peak write load. Send conflicting writes to the same key from multiple clients simultaneously. These push the system into states that normal operation reaches only after months of uptime.

Only normal operations? You test the happy path. Add adversarial inputs and you test the validation path. Add nemesis operations and you test the recovery path. Bugs overwhelmingly live in recovery paths. Your alphabet needs all three letters.

```rust
let roll = ctx.random().random_range(0..100);
match roll {
    0..60 => {
        // Normal: production-like read/write mix
        self.do_normal_operation(ctx).await?;
    }
    60..80 => {
        // Adversarial: boundary values, empty inputs, collisions
        self.do_adversarial_operation(ctx).await?;
    }
    80..100 => {
        // Nemesis: conflict storms, concurrent mutations, stress
        self.do_nemesis_operation(ctx).await?;
    }
}
```

The percentages are tunable. Start with a heavy normal-operation bias and adjust based on what your `assert_sometimes!` statements tell you about coverage.

## Invariant Patterns

Generating interesting operations is half the problem. The other half is knowing whether the system **behaved correctly** under those operations. Four patterns have proven reliable across many simulation workloads.

**Reference models** maintain an in-memory expected state and compare after operations. We saw this in the [workload chapter](./03-writing-workload.md): a `BTreeMap` mirrors what the server should contain, updated on every write, compared on every read. Use `BTreeMap`, not `HashMap`. Deterministic iteration order makes failures reproducible.

**Conservation laws** check quantities that must remain constant. Total money in a transfer system. Total record count across shards. Messages sent minus messages received. The [invariants chapter](./17-invariants.md) showed this with the banking conservation law.

**Structural integrity** validates data structures after chaos. Traverse a linked list and verify no cycles. Walk a B-tree and check balance at every level. Count children and verify parent pointers. These catch corruption that reference models miss because they operate at a different abstraction level.

**Operation logging** resolves commitment uncertainty. When a write fails with a network error, did it commit or not? Log the intent alongside the mutation. After recovery, read back the state and reconcile against the log. Essential for workloads that operate through unreliable networks, which is all simulation workloads.

## Assertions as Memos

Lawrie Green offers a perspective on assertions that changes how you think about placing them. Assertions serve two audiences simultaneously.

They inform the **computer** about expected behavior. In moonpool, `assert_sometimes!` tells the explorer "this state is interesting, branch from here." `assert_always!` tells the runner "if this fails, stop and report." Assertions are active participants in the search.

They document the **developer's mental model**. When you write `assert_always!(balance >= 0, "balance should never go negative")`, you are recording a belief about how the system works. The assertion is a memo to your future self and to every developer who reads the code after you.

When an assertion fails on **correct** code, the mental model was wrong. That is itself a critical finding. Maybe the behavior is fine and the assertion needs updating. Maybe it reveals a design assumption that will cause trouble later. Either way, the mismatch between belief and reality is worth knowing about.

This dual purpose means assertions belong everywhere a developer has an opinion about what should happen. On the error path. On the recovery path. On the "this should never happen" path that definitely will happen in simulation.

## Generate Sufficient Workload

A single client sending sequential requests will never trigger the race conditions where distributed bugs hide. The system needs enough concurrent activity for interesting interactions to emerge.

Define **all** actions the system can take. Not just the obvious ones. Multi-step workflows: "begin transaction, write three keys, read one back, commit." Interacting operations: "two clients write the same key simultaneously." Failure-spanning operations: "write during a partition, read after recovery."

Then run many instances concurrently:

```rust
SimulationBuilder::new()
    .processes(3, || Box::new(KvServer::new()))
    .workloads(5, || Box::new(KvWorkload::new(200)))
    .run()
    .await
```

Five workloads running 200 operations each, against three servers, with chaos enabled. The combinatorial interactions between concurrent operations, across servers experiencing faults, produce the complex interleavings where bugs live.

There is a deeper principle here. Individual test cases scale **multiplicatively** with system complexity. A 300-line feature can require 10,000 lines of manual test code for combinatorial coverage. Test generators scale **linearly**. Adding one variant to your operation enum covers all new combinations with that operation. You write one line. The simulator explores thousands of new scenarios.

```rust
enum Operation {
    Get { key: String },
    Set { key: String, value: Vec<u8> },
    Delete { key: String },
    Scan { start: String, end: String },
    // Adding this one variant automatically generates
    // combinations with every other operation, every
    // fault type, and every timing interleaving.
    CompareAndSwap { key: String, expected: Vec<u8>, new: Vec<u8> },
}
```

This is the real payoff of simulation testing. Not that each individual test is better, but that the **cost of coverage** changes from exponential to linear. One well-designed workload, with a complete operation alphabet, strong invariants, and enough concurrency, finds more bugs than a thousand hand-written test cases.
