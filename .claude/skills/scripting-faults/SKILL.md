---
name: scripting-faults
description: Write a custom moonpool FaultInjector for targeted, scripted faults - the FaultContext API (crash, restart, reboot, partition, heal_partition, reboot_random, reboot_machine), reading workload state to time an injection, chaos_shutdown, and registering it with fault_factory. Use when a scenario needs a specific kill/restart sequence or partition rather than random attrition, when building a scripted corpus case, or when a bug needs a precise interleaving to reproduce.
---

# Scripting faults

Random attrition (`Chaos::Attrition`) finds what the swarm stumbles into. A
`FaultInjector` scripts what you already know you want: kill the leader the
moment it commits, partition two nodes for exactly one election, restart a
node once the workload reports it is behind. Both live on the same builder and
compose.

## The trait

```rust
use async_trait::async_trait;
use moonpool_sim::{FaultContext, FaultInjector, RebootKind, SimulationResult, TimeProvider};

struct KillLeaderOnce;

#[async_trait]
impl FaultInjector for KillLeaderOnce {
    fn name(&self) -> &str { "kill-leader-once" }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        // Wait for a fact the workload published, never for wall-clock time.
        let leader: String = loop {
            if let Some(ip) = ctx.state().get::<String>("leader") { break ip; }
            ctx.time().sleep(std::time::Duration::from_millis(50)).await?;
        };
        ctx.crash(&leader).await?;                 // held down until restart()
        ctx.time().sleep(std::time::Duration::from_secs(2)).await?;
        ctx.restart(&leader).await?;
        Ok(())
    }
}

// On the builder: a factory, so exploration and the canary get a fresh injector per timeline.
// .fault_factory(|| Box::new(KillLeaderOnce))
```

## `FaultContext` (`crates/moonpool-sim/src/runner/fault_injector.rs`)

| Need | Call |
|---|---|
| a process's published state | `ctx.state().get::<T>(key)` |
| who is up | `ctx.process_ips()`, `ctx.is_dead(ip)`, `ctx.dead_count()`, `ctx.ips_in_group(name)`, `ctx.group_registry()` |
| kill and hold | `ctx.crash(ip)` then `ctx.restart(ip)` |
| kill and auto-restart | `ctx.reboot(ip, RebootKind::{Graceful, Crash, CrashAndWipe})`, `ctx.reboot_with_delays(ip, kind, &recovery_ms, &grace_ms)` |
| a random or tagged victim | `ctx.reboot_random(kind)`, `ctx.reboot_tagged(key, value, kind)` |
| a whole failure domain | `ctx.reboot_machine(id, kind)`, `ctx.reboot_domain(level, id, kind)`, `ctx.machine_registry()` |
| a directional split | `ctx.partition(a, b)`, `ctx.heal_partition(a, b)`, `ctx.is_partitioned(a, b)` |
| time and randomness | `ctx.time()`, `ctx.random()` (the shared seeded stream) |
| the chaos window closing | `ctx.chaos_shutdown()` — stop injecting when it fires so the recovery tail is quiet |

## Rules

- **Respect the chaos window.** After `chaos_duration` the world enters
  recovery mode: no new simulator faults, partitions healed, persistent damage
  kept. A scripted injector that keeps killing after `chaos_shutdown()` turns
  the liveness tail into noise.
- **Draw from `ctx.random()`, never `rand::thread_rng()`.** The injector's
  choices are part of the seed.
- **Bound `max_dead` yourself.** `crash` holds a node down; two crashes of a
  three-node quorum is a legitimate scenario only if the check expects it.
- **Pair each scripted fault with a coverage claim** in the workload or
  invariant (`assert_sometimes!("recovered after leader crash")`), otherwise
  nothing proves the script did what it says.
- **State in the injector is per timeline.** The factory recreates it, so keep
  a script's "already fired" flag in the struct, not in a static.

Book: `book/src/part3-building/07-chaos.md`, `09-attrition.md`,
`10-network-faults.md`; tests: `crates/moonpool-sim/tests/scripted_faults.rs`,
`recovery_mode.rs`.
