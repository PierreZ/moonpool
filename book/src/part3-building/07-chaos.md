# Chaos in Moonpool

<!-- toc -->

## Make Rare Bugs Common

Real distributed systems fail in ways that are individually rare and collectively certain. A partition happens once a month. A disk tears a write once a year. A process dies during recovery once in a thousand deployments. Multiply enough small probabilities across enough nodes and enough time, and the question is not **if** but **when**.

The principle behind chaos in moonpool is simple: **amplify failure probabilities so that rare bugs become common**. A monthly partition happens every few seconds in simulation. A yearly torn write happens every hundred writes. A crash at the worst possible moment happens on the seed that chose that moment.

This is not reckless. Because every fault is drawn from the seed, the bug found at high probability is the same bug that would appear at low probability in production, found a thousand times faster, with a replay attached.

## Two Sides of a Fault

Faults enter the simulation from two sides, and the split matters.

The **environment** side is the builder. `SimulationBuilder::enable_chaos` takes a list of `Chaos` surfaces. The runner samples each one per seed, runs it during the chaos window, and switches it off when the window closes. The code under test never knows a surface exists.

The **code** side is the shipped program. `buggify!` makes a rare-but-legal decision of the code's own (a tiny batch, a skipped resend). `hint!` names a moment where an environmental fault would be interesting (a batch is durable but not yet sent), and the simulator decides whether to strike. Both are no-ops in production, so the code that ships is the code that runs under chaos.

## Every Chaos Surface

`enable_chaos` accepts any mix of these variants. Each one is sampled by a `ChaosMode` where it has one.

| Variant | What it does | Chapter |
|---------|--------------|---------|
| `Chaos::Network(mode)` | Clogs, partitions, bit flips, random closes, connect failures, clock drift, buggified delays, slow pairs, black holes, at the TCP connection level. | [Network Faults](10-network-faults.md) |
| `Chaos::Storage(mode)` | Corruption, EIO, misdirected and phantom writes, sync failures, short transfers, lost directory entries, crash damage, stalls and throttles, disk failure. | [Storage Faults](11-storage-faults.md), [Aiming Storage Faults](33-aimed-faults.md) |
| `Chaos::Attrition { config, mode }` | Reboots processes one at a time (or one failure domain at a time) under a `max_dead` budget. One injector per entry, so each process group can have its own regime. | [Attrition](09-attrition.md) |
| `Chaos::Outage { config, mode }` | A correlated outage: every process of the named groups loses power in the same tick, and each comes back on its own delay. At most once per run. | [Correlated Outages](30-outages.md) |
| `Chaos::BuggifyKnobs` | A modifier. On surfaces already enabled, some seeds push a knob to an extreme (a disk at 100 IOPS, 1% of sends corrupted). Never switches a surface on by itself. | [Buggify](08-buggify.md#spiking-config-knobs) |
| `Chaos::Cpu(mode)` | The CPU model: every poll of a process task costs CPU time on the process's one core, so a busy machine answers late. | [CPU Time and Gray Failures](26-cpu-and-gray-failure.md) |
| `Chaos::GrayFailure { config, mode }` | One or more machines stay alive and slow for a long window: CPU, disk or network multiplied by a factor. At most `max_slow` at once. | [CPU Time and Gray Failures](26-cpu-and-gray-failure.md) |

Two surfaces are not in the list because they are not sampled per seed. Custom fault injectors are registered with `fault_factory` and script whatever they want through `FaultContext`, see [Custom Fault Injectors](31-custom-faults.md). Replicated fault patterns are a shaping of `Chaos::Storage`, registered with `replicated_storage_faults`, see [Aiming Storage Faults](33-aimed-faults.md).

```rust
use moonpool_sim::{Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, SimulationBuilder};

SimulationBuilder::new()
    .processes(3, || Box::new(MyProcess))
    .workload(MyWorkload)
    .enable_chaos([
        Chaos::Network(ChaosMode::Swarm),
        Chaos::Storage(ChaosMode::Swarm),
        Chaos::Attrition {
            config: Attrition {
                max_dead: 1,
                prob_graceful: 0.3,
                prob_crash: 0.5,
                prob_wipe: 0.2,
                recovery_delay_ms: None,
                grace_period_ms: None,
                scope: AttritionScope::PerProcess,
                victims: AttritionVictims::Any,
            },
            mode: ChaosMode::Swarm,
        },
        Chaos::BuggifyKnobs,
    ])
    .chaos_duration(Duration::from_secs(30))
    .run()?;
```

A surface absent from the list stays off. Its configuration is the default: low latency, no faults.

## ChaosMode: Random or Swarm

Every sampled surface takes one of two modes.

**`ChaosMode::Random`** turns on the whole surface and draws its intensities per seed. Every network family is a little bit on, on every seed. For attrition and outages, `Random` runs the configured regime as written.

**`ChaosMode::Swarm`** turns on a random **subset** of the surface's fault families per seed and fully disables the rest. One seed is clog-only. Another is partition plus bit flip. Another is the all-off baseline. For attrition, `Swarm` draws the reboot regime itself, including the never-reboot regime. For outages, `Swarm` drops the outage on half the seeds.

Swarm is the mode we recommend. [Swarm Testing: Masks and Budgets](28-swarm-testing.md) explains why, lists every mask and budget, and shows how one workload under swarm covers more than many workloads without it.

## The Chaos Window

Every surface runs inside the window `chaos_duration` sets. The builder refuses a campaign that registers attrition, an outage, a gray failure or a `fault_factory` without one: `run()` returns `SimulationError::InvalidConfiguration` instead of never running the injector.

When the window closes, the runner ends chaos in one step:

1. It cancels `FaultContext::chaos_shutdown`, so injector loops wind down.
2. It calls `SimWorld::enter_recovery_mode`, which turns off every configuration-driven network, storage and block-device fault family and heals the partitions it holds.
3. It calls `moonpool_buggify::buggify_enter_recovery`, so disruptive buggify sites and every `hint!` fall silent.

The cutoff bounds **fault generation**, nothing else. Corrupted sectors stay corrupted. Lost writes stay lost. A process already killed restarts on its own schedule. The cluster is not healthy at the cutoff. The environment has stopped making it worse, and recovering is now the system's job. The quiet tail between the cutoff and workload completion is where a rebooted process rejoins and a cluster reconverges. Give the workload enough simulated time there, then assert on convergence.

## Determinism Is Not Negotiable

Every chaos decision is a draw on the one simulation stream. The same seed produces the same faults in the same order at the same times. So:

- A failing seed is a permanent, reproducible bug report.
- You debug a failure by replaying it with tracing turned up.
- Fixing a bug and re-running the seed verifies the fix.
- Different seeds explore different fault combinations on their own.

The draw order is a contract. Network and storage profiles are drawn before the world reseeds the stream. The operation-alphabet mask comes next, then attrition regimes in registration order, then outages, then the CPU model and gray failures. Moving one draw shifts every seed, which is why the runner pins the order and why a surface you leave off costs no draw. The [Fault Reference](../appendix/04-fault-reference.md) lists every family, its default rate and its mask bit.
