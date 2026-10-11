# moonpool-sim

Deterministic simulation for distributed systems in Rust, after
[FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html)
and [Antithesis](https://antithesis.com/).

moonpool-sim runs a whole cluster in one OS thread: server processes, test
clients, the network between them and the disks under them. It runs on
logical time, from one seed. The same seed gives the same run, bit for bit,
so a bug found after a million simulated operations replays on demand.

## Why

Traditional tests ask: "did we break what used to work?" That is
**prevention** testing. Simulation asks: "what else is broken that we have
not found yet?" That is **discovery** testing.

Bugs in distributed systems hide in rare combinations: a crash during a
sync, a partition during an election, a slow disk on the leader. Production
meets these combinations after months. The simulator makes them happen
every second, and the seed makes each one reproducible.

- **Deterministic.** One seeded random stream drives every decision: task
  order, `select!` branch order, latencies, faults, and the code's own
  draws.
- **Fast.** Time jumps to the next event. A simulated hour of heartbeats
  passes in milliseconds.
- **Hostile.** Packets are lost and corrupted, links partition, writes tear,
  processes crash, racks lose power, machines go slow.

## Same code in production and simulation

The code under test is the code you ship. It depends on the provider traits
of `moonpool-core`: `TimeProvider`, `TaskProvider`, `NetworkProvider`,
`RandomProvider`, `StorageProvider` and `SystemProvider`. Production passes
`TokioProviders`; the simulation passes `SimProviders`. There is no
`SimDisk` or test-only hook in the shipped code.

Shipped code can also name its own interesting moments. These calls are
no-ops in production:

- `buggify!()` takes a rare path on some seeds.
- `hint!("mid-append").await` names a moment. The simulator may reboot the
  process exactly there, under the seed's attrition budget.

## Quick start

A simulation has three parts:

1. A `Process`: the system under test. It boots fresh on every reboot.
2. A `Workload`: the test driver. It survives reboots and judges the result.
3. A `SimulationBuilder`: topology, chaos and seeds.

```rust
use std::time::Duration;

use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, SimulationBuilder,
};

let reboots = Attrition {
    max_dead: 1,
    prob_graceful: 0.2,
    prob_crash: 0.7,
    prob_wipe: 0.1,
    recovery_delay_ms: None,
    grace_period_ms: None,
    scope: AttritionScope::PerProcess,
    victims: AttritionVictims::Any,
};

let report = SimulationBuilder::new()
    .processes(3..=5, || Box::new(MyNode::new()))
    .workload_factory(|| Box::new(MyClient::default()))
    .enable_chaos([
        Chaos::Network(ChaosMode::Swarm),
        Chaos::Storage(ChaosMode::Swarm),
        Chaos::Attrition { config: reboots, mode: ChaosMode::Swarm },
    ])
    .chaos_duration(Duration::from_secs(30))
    .until_coverage_stable(10, 1_000)
    .run()?;

report.eprint();
assert!(!report.is_failure());
```

The crate documentation on [docs.rs](https://docs.rs/moonpool-sim) has a
complete, compiling version with an echo server and a client.

To debug a failing seed, pin it: `.set_debug_seeds(vec![seed])` and
`.set_iterations(1)`. Raise `.trace_level(...)` to see more.

## Chaos

`enable_chaos` takes a list of surfaces. Each surface has a mode:
`ChaosMode::Random` turns the whole surface on with per-seed intensities;
`ChaosMode::Swarm` turns on a random **subset** of its fault families per
seed, so sharp single-fault configurations happen often.

| Surface | Faults |
|---------|--------|
| `Chaos::Network` | Partitions, clogs, bit flips, random closes, connect failures, clock drift, buggified delays, slow IP pairs, black holes |
| `Chaos::Storage` | Corruption, `EIO`, misdirected and phantom writes, sync failures, short transfers, crash damage, disk stalls, disk failure |
| `Chaos::Attrition` | Graceful reboots, crashes and wipes under a `max_dead` budget, per process group |
| `Chaos::Outage` | A whole group loses power at once; `OutageVeto` lets the harness defer it |
| `Chaos::Cpu` | The CPU gate: each poll of a process costs time on its one core |
| `Chaos::GrayFailure` | Machines stay alive but slow for a long window: disk, network, and CPU when `Chaos::Cpu` is on |
| `Chaos::BuggifyKnobs` | Pushes the enabled network and storage knobs to extremes |

More controls:

- `network_fault_mask` and `storage_fault_mask` remove fault families a
  harness cannot judge, without changing any random draw.
- `swarm_operations` gives each seed a random subset of the workload's own
  operations (`swarm_op_enabled`).
- `replicated_storage_faults` spreads disk damage over failure domains, so
  no replicated record is damaged on every replica.
- `fault_factory` adds a custom `FaultInjector` for scripted faults.
- `SimContext::crash_self` lets a process crash itself at an exact point of
  its protocol.

`chaos_duration` bounds fault **generation**. When it ends, no new faults
start, but the damage stays: the system must recover in the quiet tail.

## Storage crash model

Each simulated file keeps the bytes of its last successful sync and the
bytes reads see. At a crash, each sector written since the last sync
resolves on its own to old, new, lost or damaged, as a real disk allows. A
lost-synced-write oracle checks that nothing synced is ever lost.

## Assertions

Fifteen Antithesis-style macros. They never panic: a failed `always` fails
the seed at the end of the iteration.

| Kind | Macros |
|------|--------|
| Boolean | `assert_always!`, `assert_always_or_unreachable!`, `assert_sometimes!`, `assert_reachable!`, `assert_unreachable!` |
| Numeric | `assert_always_greater_than!`, `assert_always_greater_than_or_equal_to!`, `assert_always_less_than!`, `assert_always_less_than_or_equal_to!`, `assert_sometimes_greater_than!`, `assert_sometimes_greater_than_or_equal_to!`, `assert_sometimes_less_than!`, `assert_sometimes_less_than_or_equal_to!` |
| Compound | `assert_sometimes_all!`, `assert_sometimes_each!` |

If an error handler exists but its `assert_sometimes!` never fires, it was
never tested. The default stop rule, `UntilCoverageStable`, keeps trying
seeds until every `sometimes` and `reachable` assertion fired and code
coverage stopped growing (capped at `max_iterations`).

## Events and invariants

Code emits ordinary `tracing` events, the same ones production
observability reads. `SimulationLayer` captures them into a timeline, and
invariants (`.invariant(...)`, `.invariant_fn(...)`) read it after every
simulation step. Injected faults appear on the same timeline as
`SimFaultEvent`s.

## Exploration

With the `exploration` feature (on by default), `enable_exploration` turns on
`moonpool-explorer`. When a seed reaches a new assertion outcome, the
explorer records a recipe of RNG call counts and seeds. Bounded workers
replay that prefix and continue it with fresh randomness. A bug comes back
as a `BugRecipe`; `replay_timeline(seed, recipe)` reproduces it. This is
assertion-guided randomized testing, not exhaustive model checking.
Exploration needs factory workloads (`workload_factory` or `workloads`).

## Feature flags

| Feature | Default | Effect |
|---------|---------|--------|
| `tokio-providers` | on | Re-exports `TokioProviders` so one test can drive the same code in production mode and in simulation |
| `exploration` | on | Fork-based exploration through `moonpool-explorer` (uses `libc`) |

With `--no-default-features` the crate builds for `wasm32`. Every chaos
surface and the assertion accounting still work.

## Documentation

- [The Sim Book](https://pierrez.github.io/moonpool/): the user guide
- [API documentation](https://docs.rs/moonpool-sim)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
