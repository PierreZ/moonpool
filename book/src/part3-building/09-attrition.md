# Attrition: Process Reboots

<!-- toc -->

## Why Processes Need to Die

A distributed system that only works while every node is healthy is a single point of failure with extra network hops. Real clusters lose nodes all the time. A rolling deployment restarts a process. A kernel panic kills one. A power failure wipes its storage. Your system must handle all three while it keeps serving requests.

Attrition is moonpool's built-in injector for that drizzle of failures. During the chaos window it waits 1 to 5 simulated seconds, picks a live victim, kills it in one of three ways, and restarts it after a recovery delay. Then it waits again. The goal is to prove the system tolerates node loss without anyone writing a test per failure.

## Three Kinds of Reboot

`RebootKind` models three real failures.

**`Graceful`** is the controlled shutdown. The process's `ctx.shutdown()` token fires. The process gets a grace period to drain buffers, close connections and flush writes. If it has not returned when the period ends, it is force-killed. Send buffers drain during the grace period (FIN semantics). Then a fresh instance boots. This models rolling deployments and well-behaved process managers.

**`Crash`** is sudden death. The process task is cancelled at the crash instant, before its connections abort, so nothing the crash disturbs can wake it again. Connections abort with no drain. Peers see connection resets. Unsynced storage resolves by the disk's crash physics. The process runs no work until its restart. This models kernel panics, OOM kills and hardware faults.

**`CrashAndWipe`** is the worst case. Same as `Crash`, and every file the process owns is deleted. The process restarts as a brand-new node. The wipe is scoped to the process's IP, so other processes keep their disks. This models a replaced machine or a lost disk.

## The Attrition Configuration

```rust
use moonpool_sim::{Attrition, AttritionScope, AttritionVictims};

Attrition {
    max_dead: 1,
    prob_graceful: 0.3,
    prob_crash: 0.5,
    prob_wipe: 0.2,
    recovery_delay_ms: Some(1000..10000),
    grace_period_ms: Some(2000..5000),
    scope: AttritionScope::PerProcess,
    victims: AttritionVictims::Any,
    wipe_budget: None,
}
```

**`max_dead`** caps the processes dead at the same time **among this injector's victims**. A process counts as dead from the instant its reboot is decided, graceful or crash, until its restart runs. Victims are drawn only from live processes, and a reboot aimed at a dead process is skipped, so one outage never schedules two restarts.

**`prob_graceful`, `prob_crash`, `prob_wipe`** are weights, not probabilities. They need not sum to 1.0. The injector normalizes them and draws a kind. `prob_wipe: 0.0` disables wipes. All three at zero means always crash.

**`recovery_delay_ms`** is the range a dead process stays dead, drawn per reboot. `None` means `1000..10000`.

**`grace_period_ms`** is the range a graceful shutdown may take, drawn per reboot. `None` means `2000..5000`.

**`scope`** is the failure domain a reboot targets. `PerProcess` kills one process. `PerMachine`, `PerZone` and `PerDatacenter` kill every collocated process together and need a `.cluster()` topology.

**`victims`** is who may die at all. `AttritionVictims::Any` draws over every server process. `AttritionVictims::group("acceptor")` and `AttritionVictims::tagged("role", "voter")` narrow the pool.

**`wipe_budget`** bounds the wiped processes of the pool that are not yet healed. `None` (the default) leaves wipes bounded by `max_dead` alone. [Rolling Wipes](#rolling-wipes-the-wipe-budget) explains it.

## Using Attrition

```rust
use std::time::Duration;
use moonpool_sim::{Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, SimulationBuilder};

SimulationBuilder::new()
    .processes(3, || Box::new(MyProcess::new()))
    .enable_chaos([Chaos::Attrition {
        config: Attrition {
            max_dead: 1,
            prob_graceful: 0.3,
            prob_crash: 0.5,
            prob_wipe: 0.2,
            recovery_delay_ms: None,
            grace_period_ms: None,
            scope: AttritionScope::PerProcess,
            victims: AttritionVictims::Any,
            wipe_budget: None,
        },
        mode: ChaosMode::Random,
    }])
    .chaos_duration(Duration::from_secs(60))
    .workload(MyWorkload::new())
    .run()?;
```

`chaos_duration` is required. `run()` returns `SimulationError::InvalidConfiguration` for a builder that registers attrition without it, instead of dropping the injector unrun. When the window closes, no new reboot is scheduled. A reboot already decided still completes: its restart is a scheduled event, so no victim is stranded down into the recovery tail. The tail is where the rebooted process rejoins and the cluster reconverges, so give the workload enough time there.

## Random or Swarm

`ChaosMode::Random` runs the regime as written on every seed. `ChaosMode::Swarm` draws the **regime** per seed, in six draws on the simulation stream:

1. `max_dead` becomes 0 on half the seeds. That is the never-reboot regime, which finds the slow leaks and timer bugs that constant restarting hides.
2. Each reboot kind is masked off on half the seeds. One kind left is a single-mode regime, such as always-crash or graceful-only.
3. The recovery-delay range is scaled to between 50% and 200% of the configured one.
4. With `.cluster()`, the scope is drawn among `PerProcess` and the correlated scopes whose domains all fit `max_dead`. A flat `.processes()` campaign keeps its scope.

The injector still makes its usual draws per reboot for kind, delay and victim. The regime costs a fixed footprint at a fixed position, so it replays with the seed. Regimes are drawn in registration order after the stream is reseeded. [Swarm Testing](28-swarm-testing.md) explains why the never-reboot seed is as valuable as the crash-heavy one.

## The max_dead Constraint

`max_dead` is the bridge between chaos and correctness. Without it, attrition could kill every node at once, which is chaos but not useful chaos. Set it to what your system claims to survive. Replication factor 3 tolerates one failure, so `max_dead: 1`. A system that needs 3 of 5 nodes uses `max_dead: 2`. Attrition then tests failures the system **should** survive, not failures that no design survives.

The budget is per victim pool. With `AttritionVictims::Any` it is the whole cluster. With a group or tag filter it is that pool alone, so "at most one dead acceptor" means exactly that, whatever happens to the matchmakers. A reboot hint from the code (see [hint!](29-hints.md)) spends the same budget: the first regime whose filter admits the hinting process decides, and a full budget refuses the hint.

## Rolling Wipes: the Wipe Budget

`max_dead` counts processes that are **dead**. A wiped process is dead only during its recovery delay. It then restarts, alive but empty, long before the system has rebuilt what it lost. So two wipes a few seconds apart each pass `max_dead: 1`, and together they can erase two of three replicas. A Paxos deployment with three matchmakers lost two of them that way, 6 s apart.

A wipe budget makes wipes roll through the pool, one healed process at a time:

```rust
use moonpool_sim::{WipeHeal, WipeLedger};

let regime = Attrition {
    max_dead: 1,
    victims: AttritionVictims::group("matchmaker"),
    ..base
}
.wipe_budget(1, WipeHeal::Signal); // at most one unhealed wipe

// In the system under test, once a wiped node has caught up:
WipeLedger::heal(ctx.state(), ctx.my_ip());
```

A wiped process is **unhealed** from the instant its wipe is decided until it heals. While the pool holds `max_unhealed` unhealed processes, a drawn `CrashAndWipe` becomes a plain `Crash`. A group wipe (a machine, zone or datacenter scope) counts every process of the group. A victim that is itself unhealed does not count twice, since wiping it again loses nothing more. The downgrade applies on every path the regime reboots through: timed reboots, failure-domain reboots, and [hints](29-hints.md).

Two ways to heal:

- **`WipeHeal::Signal`**: the harness declares a process healed with `WipeLedger::heal(state, ip)`. It returns whether the process was unhealed, so calling it on every catch-up is safe. Only the system knows when a replaced node has caught up. A process never signaled stays unhealed for the rest of the run, so every later wipe in its pool is a crash.
- **`WipeHeal::After(duration)`**: healed that long in simulated time after the wiped process restarts. A signal heals it earlier.

`WipeLedger::unhealed(state)` lists the processes wiped and not yet signaled healed.

The budget costs no randomness. The kind is drawn as before, then judged against the ledger, so a seed draws the same values with or without a budget. Swarm mode copies `wipe_budget` through and draws nothing for it. Every wipe enters the ledger, including a scripted `FaultContext::reboot(ip, RebootKind::CrashAndWipe)` and a process that crashes itself with a wipe. A scripted wipe is never downgraded, but it counts against a budgeted pool it lands in.

## Role-Aware Victims: One Regime per Group

A cluster rarely has one kind of node. Once you register [process groups](27-process-groups.md), uniform attrition draws its victim over **every** process. On a seed with three acceptors and five spares, most kills land on the spares, and the consensus tier you meant to stress barely notices. Tell attrition who it may kill:

```rust
Attrition {
    max_dead: 1,
    victims: AttritionVictims::group("acceptor"),   // never a spare
    ..base
}
```

The filter costs no randomness. The victim is drawn uniformly over the **eligible** processes, so a filter that excludes nothing leaves every seed's draw schedule untouched, and a filter that leaves nothing eligible skips the round without a draw. It applies to every scope: a machine-scoped reboot only picks machines that host an eligible process and only kills the eligible processes on it.

Because the budget is per pool, each group can have its own regime. Every `Chaos::Attrition` entry adds one independent injector:

```rust
.enable_chaos([
    Chaos::Attrition {
        // Acceptors: at most one down at a time, regime swarmed per seed.
        config: Attrition { max_dead: 1, victims: AttritionVictims::group("acceptor"), ..base.clone() },
        mode: ChaosMode::Swarm,
    },
    Chaos::Attrition {
        // Matchmakers: hammered independently, up to three down at once.
        config: Attrition { max_dead: 3, victims: AttritionVictims::group("matchmaker"), ..base },
        mode: ChaosMode::Random,
    },
])
```

The two injectors run side by side and never block each other. In the report each carries its filter in its name (`attrition[group=acceptor]`, `attrition[tag=role=voter]`), so a failing seed says which regime was live.

## Failure Domains: Correlated Reboots

`max_dead: 1` protects you from two random deaths at once. Real outages are rarely random and rarely one node at a time. A host reboots and every process pinned to it dies together. These are **correlated failures**, and they break quorum logic because the nodes that die were never independent.

Flat IPs cannot express this, so moonpool uses FoundationDB's hierarchy: datacenter, zone, machine, process. You describe it with `.cluster()` and a `LocalityConfig`, which [Process Groups, Tags and Topology](27-process-groups.md) covers. With a topology in place, `scope` earns its keep:

```rust
.enable_chaos([Chaos::Attrition {
    config: Attrition {
        max_dead: 2,  // one machine's worth of processes
        scope: AttritionScope::PerMachine,
        ..base
    },
    mode: ChaosMode::Random,
}])
```

`PerMachine` picks a machine and reboots **every process on it in the same instant**, as a host kernel panic would. `PerZone` does the same one level up, and `PerDatacenter` takes a region. `max_dead` still counts processes, and a group reboot is atomic against it: the injector only kills a domain when the whole group fits the remaining budget. With two processes per machine, `max_dead: 2` lets exactly one machine be down and never leaves a machine half-dead. A domain with no live eligible process is never drawn. Without a topology the correlated scopes do nothing.

A machine reboot and a rack power loss look alike but differ in one way: attrition stays under `max_dead`, so a quorum always survives. A fault that takes a **whole group** at once, past any budget, is `Chaos::Outage`, in [Correlated Outages and Self-Crashes](30-outages.md). A scripted sequence ("crash node X, wait for the workload, bring X back") is a custom injector, in [Custom Fault Injectors](31-custom-faults.md).
