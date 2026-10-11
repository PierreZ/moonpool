# Swarm Testing: Masks and Budgets

<!-- toc -->

## Passive Suppression

`ChaosMode::Random` sets **every** fault family of a surface to a random non-zero rate. Clogging is a little on, partitions are a little on, bit flips are a little on, all at once, on every seed. That sounds thorough. It is the opposite.

The problem is called **passive suppression**, and it comes from "Swarm Testing" by Groce, Zhang, Eide, Chen and Regehr (ISSTA 2012). When every feature is always slightly active, the features crowd each other out. To find a clogging bug you often need clogging cranked hard, for a sustained stretch, with nothing else interfering. If partitions keep tearing down the connection you were trying to clog, you never drive clogging deep enough. The all-on configuration explores a narrow band in the middle of the space and never visits the extremes where bugs live. In the paper, swarm testing found **42% more distinct compiler crashes** than a hand-tuned default configuration.

The fix is counterintuitive. For each run, turn **off** a random subset of features entirely. One seed is clog-only. Another is partition plus bit flip. Another is the all-off baseline. Across many seeds you reach the single-family extremes the all-on config never reaches. Will Wilson's Antithesis talks made the same point for distributed systems.

## ChaosMode::Swarm, Surface by Surface

`ChaosMode::Swarm` applies the paper per surface. Each surface starts from its `Random` profile, then draws one coin per family and switches the losers fully off. The all-off subset is allowed on purpose: a no-fault run is a valid configuration, and it is where slow leaks show.

| Surface | What `Swarm` draws per seed |
|---------|-----------------------------|
| `Chaos::Network(Swarm)` | One coin each for clog, partition, bit flip, random close, connect failure, clock drift, buggified delay, pair latency and black hole. |
| `Chaos::Storage(Swarm)` | One coin each for read and write corruption, crash damage, misdirected reads and writes, phantom writes, sync failure, disk stall, disk throttle, disk failure, short transfer, directory-entry loss, read EIO and write EIO. |
| `Chaos::Attrition { mode: Swarm, .. }` | Six draws: `max_dead` to 0 on half the seeds (the never-reboot regime), one mask per reboot kind, a recovery-delay range scaled to 50% to 200% of the configured one, and with `.cluster()` a failure scope whose domains fit `max_dead`. All three kinds masked off is the always-crash regime. |
| `Chaos::Outage { mode: Swarm, .. }` | Two draws: no outage on half the seeds, and no straggler on half the rest. |
| `Chaos::Cpu(Swarm)` | The CPU model on for half the seeds, with per-seed costs. |
| `Chaos::GrayFailure { mode: Swarm, .. }` | Each kind (CPU, disk, network) masked per seed. |

Durations, cooldowns and strategies stay as sampled. They are inert once their family is off.

```rust
SimulationBuilder::new()
    .enable_chaos([
        Chaos::Network(ChaosMode::Swarm),
        Chaos::Storage(ChaosMode::Swarm),
        Chaos::Attrition { config: regime, mode: ChaosMode::Swarm },
    ])
```

Every coin comes from the one simulation stream, seeded for the iteration. Network draws first, then storage, both before the world reseeds the stream. The operation-alphabet mask comes next, then attrition regimes in registration order, then outages, the CPU model and gray failures. A coin is drawn whether or not the family wins, so the count per surface is fixed. Same seed, same subset, every time: a replay rebuilds the subset before the counted run starts, and a fork-explorer recipe never lands on a configuration draw.

## Caller Masks

Swarm picks a subset the seed likes. Sometimes you need a subset you like: the system under test does not handle a family yet, or you are hunting one family and want the rest silent. `network_fault_mask` and `storage_fault_mask` retain only the families you name:

```rust
use moonpool_sim::{NetworkFault, NetworkFaultMask, StorageFault, StorageFaultMask};

SimulationBuilder::new()
    .enable_chaos([Chaos::Network(ChaosMode::Swarm), Chaos::Storage(ChaosMode::Swarm)])
    // Hunting a replication bug: no corrupted frames, no lying syncs.
    .network_fault_mask(NetworkFaultMask::all().without(NetworkFault::BitFlip))
    .storage_fault_mask(
        StorageFaultMask::all()
            .without(StorageFault::SyncFailure)
            .without(StorageFault::DiskFailure),
    )
```

Both masks have the same shape: `all()`, `none()`, `with(fault)`, `without(fault)` and `contains(fault)`. The default is `all()`. A mask is applied after profile sampling and after `Chaos::BuggifyKnobs`, right before the world is built, and it consumes no randomness. Adding or removing a mask shifts no draw, so recipes and replays stay valid. A family a mask removes is never chosen, whatever the seed's coin said.

The `NetworkFault` families are `Clog`, `Partition`, `BitFlip`, `RandomClose`, `ConnectFailure`, `ClockDrift`, `BuggifiedDelay`, `PairLatency` and `BlackHole`. The `StorageFault` families are `Corruption`, `Eio`, `Misdirect`, `PhantomWrite`, `SyncFailure`, `ShortTransfer`, `DirEntryLoss`, `CrashDamage`, `Degradation` and `DiskFailure`. Partial reads and writes are TCP behaviour driven by buggify, not a family, so no mask touches them.

## Budgets

A mask says which families may strike. A budget says how much damage may stand at once. Every built-in injector carries one:

| Budget | Where | What it bounds |
|--------|-------|----------------|
| `max_dead` | `Attrition` | Dead processes among the regime's victim pool. Swarm sets it to 0 on half the seeds. |
| `max_slow` | `GrayFailure` | Machines slow at the same time. |
| `probability` | `Outage` | Whether a seed has an outage at all. One outage per run at most. |
| `tolerance` | `ReplicatedFaults` | Failure domains holding damage to one replicated record at once. |

Budgets keep chaos survivable. A system with replication factor 3 tolerates one failure, so `max_dead: 1`. A budget the simulator cannot see belongs to the harness: a `HintVeto` refuses reboot hints and an `OutageVeto` holds an outage back, both published in the run's state. [hint!](29-hints.md) and [Correlated Outages](30-outages.md) show each one.

## Swarm the Operation Alphabet

The same suppression happens inside a workload. A bounded queue driven by a 50/50 push and pop mix is a random walk that takes tens of millions of steps to fill. `pop` keeps undoing what `push` does, and the overflow bug is never reached. The fix is the same: enable a random subset of operations per seed.

```rust
SimulationBuilder::new()
    .workload_factory(|| Box::new(QueueWorkload::new()))
    .swarm_operations()
```

With `.swarm_operations()` the runner draws a 256-bit mask right after seeding, one bit per `u8` operation id, four draws. `moonpool_sim::swarm_op_enabled(op_id)` reads one bit, consumes no randomness, and gives the same answer every time. Without the call every operation reports enabled, so a non-swarm run is byte-identical to one with no swarm code.

```rust
// Cache the subset once per run. Fall back to the full alphabet if the
// seed disabled everything, so the workload always has something to do.
let enabled: Vec<u8> = (0..NUM_OPS).filter(|&i| swarm_op_enabled(i)).collect();
let enabled = if enabled.is_empty() { (0..NUM_OPS).collect() } else { enabled };

// One draw per step, folded into the subset. A resample loop would make
// the draw count vary, and the explorer keys replay on that count.
let raw = moonpool_sim::sim_random::<u64>();
let op = enabled[(raw % enabled.len() as u64) as usize];
```

The dungeon example in `crates/moonpool-sim-examples/src/dungeon.rs` is the reference, and `crates/moonpool-sim/tests/swarm_op_alphabet.rs` shows an assertion that only fires under the swarm. [Designing Workloads That Find Bugs](19-designing-workloads.md#swarm-the-alphabet) has the full pattern.

## Named Sites as a Swarm Axis

Buggify is a swarm too: every `buggify!` location is on for half the seeds and off for the rest. The coins are independent, which is what you want for scattered points and not what you want for a scenario. A harness that needs "hold the journal **and** withhold GC requests" on the same seed waits for the product of two coins.

`buggify_named!` keys a site by a label, and `set_activation` decides it for the run:

```rust
// Shipped code.
if buggify_named!("hold journal", 1.0) { /* ... */ }
if buggify_named!("withhold gc requests", 1.0) { /* ... */ }

// Harness, at the start of the seed: one scenario draw decides both.
let scenario = ctx.random().random_bool(0.5);
moonpool_sim::set_activation("hold journal", scenario);
moonpool_sim::set_activation("withhold gc requests", scenario);
```

A site at rate `1.0` turns activation into the per-seed decision. A site the harness never sets keeps its own coin. `buggify_init` clears every decision at the next seed. [Buggify](08-buggify.md#named-points-a-harness-can-force) has the rules.

## One Workload, Many Behaviours

A common reflex is to write one workload per scenario: a partition workload, a crash workload, a slow-disk workload, a no-fault workload. Each one is a file to maintain, a report to read, and a fixed configuration that explores one point in the space.

Swarm turns the reflex around. Write **one** workload that exercises the whole operation alphabet and checks every invariant. Enable every surface with `Swarm`. Now each seed is a scenario the harness never had to name: seed 7 is partitions and crash-only attrition with pop disabled, seed 8 is a no-fault baseline with the CPU model on, seed 9 is slow disks and a rack outage. The combinations multiply across seeds, and the ones that find bugs are the ones no one would have written.

The per-seed decisions are visible, so a failing seed explains itself. Each attrition injector carries its filter in its name (`attrition[group=acceptor]`). Every fault the engine injects lands in the timeline as a `sim_fault` event with a `kind` field. The `assert_sometimes_each!` buckets under `attrition_reboot_kind` and `hint_struck` say which regime was live. Pin the seed with `set_debug_seeds`, and the swarm rebuilds the same subset from the seed before the first event runs.

Separate workloads still earn their place when the **invariants** differ, not when the faults do. A workload that checks linearizability and a workload that checks a compaction schedule are two workloads. A workload that checks linearizability under partitions and another under crashes is one workload under swarm.
