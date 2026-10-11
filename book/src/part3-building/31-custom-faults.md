# Custom Fault Injectors

<!-- toc -->

## When the Built-Ins Are Not Enough

Attrition, outages, network and storage chaos are sampled per seed. They are the right tool for "break things at random and see what survives". A scripted sequence is a different need: crash node X, let the workload make progress, bring X back, and check that X catches up. Or: wait until every replica has written its segment, then corrupt three bytes on one of them. For that you implement `FaultInjector`.

```rust
use std::time::Duration;
use async_trait::async_trait;
use moonpool_sim::{FaultContext, FaultInjector, RebootKind, SimulationError, SimulationResult, TimeProvider};

struct RollingRestart;

#[async_trait]
impl FaultInjector for RollingRestart {
    fn name(&self) -> &str { "rolling_restart" }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        // Restart each process in order, with time to recover between each.
        for ip in ctx.process_ips() {
            ctx.reboot(&ip, RebootKind::Graceful)?;
            ctx.time().sleep(Duration::from_secs(15)).await
                .map_err(|e| SimulationError::InvalidState(e.to_string()))?;
            if ctx.chaos_shutdown().is_cancelled() {
                break;
            }
        }
        Ok(())
    }
}
```

`name()` labels the injector in the report. `inject()` runs as one task for the chaos window. It must watch `ctx.chaos_shutdown()` and return once the token is cancelled.

## Registering a Factory

An injector is registered through a **factory**, never as an instance:

```rust
SimulationBuilder::new()
    .processes(3, || Box::new(MyProcess))
    .workload_factory(|| Box::new(MyWorkload))
    .fault_factory(|| Box::new(RollingRestart))
    .chaos_duration(Duration::from_secs(30))
```

The runner builds a fresh injector from the factory for every root seed and every explored continuation. A mutated instance could not be rewound for a second timeline, so scripted sequences replay exactly from the seed plus recipe, with in-process exploration and forked workers alike. An injector that must report back (an observation log) captures an `Arc` in the closure.

A fault factory requires `chaos_duration`. `run()` returns `SimulationError::InvalidConfiguration` for a builder that registers one without it. Several factories run side by side, together with the built-in injectors `enable_chaos` adds.

## The FaultContext

`FaultContext` is what an injector sees. Unlike `SimContext`, it exposes the fault primitives a workload must not touch.

| Method | What it does |
|--------|--------------|
| `process_ips()` | Every live server process IP, read now (spawned included, retired excluded). `ips_in_group(name)` narrows to one group, ascending. |
| `group_registry()`, `machine_registry()` | Copies of the live group and failure-domain registries. `membership()` and `membership_version()` read the live set itself. |
| `time()`, `random()` | The simulated time and the seeded random provider. |
| `state()` | The per-iteration `StateHandle` workloads and processes share. |
| `chaos_shutdown()` | The token cancelled when the chaos window closes. |
| `reboot(ip, kind)` | Kill and restart after a drawn delay. `reboot_with_delays` takes the delay ranges. `reboot_random`, `reboot_tagged`, `reboot_machine` and `reboot_domain` pick the victims. |
| `crash(ip)` | Force-kill with **no** restart scheduled. The node stays down until `restart`. |
| `crash_for(ip, down)` | Force-kill and restart after exactly `down`. No draw. |
| `restart(ip)` | Boot a fresh instance. On a live process it is an in-place restart. |
| `dead_count()`, `is_dead(ip)` | The budget every injector shares. |
| `partition(a, b)`, `heal_partition(a, b)`, `is_partitioned(a, b)` | A bidirectional partition that holds until healed. |
| `set_slowness(ip, s)`, `slowness(ip)`, `cpu_model()` | A gray failure by hand. |
| `durable_bytes(ip, path)` | The durable bytes of one process's file, to read a layout before aiming. No draw, no simulated time. |
| `corrupt_bytes(ip, path, range)` | Mutate exactly those bytes, durably, on that process's disk alone. |
| `fail_file_with_eio(ip, path, sectors, target)`, `clear_file_eio(ip, path, target)` | EIO on reads, writes or both, for a sector range of one file. |

The storage methods are the subject of [Aiming Storage Faults](33-aimed-faults.md). The reboot methods follow the rules in [Attrition](09-attrition.md): a process counts as dead from the decision, a dead process is skipped, and a kill lands one scheduler tick after the call.

## Scripted Lifecycle Faults

`reboot()` combines a kill with a timer-based restart, which is right for random attrition and wrong for a script. `crash` and `restart` split the two, and `state()` lets the script wait on a workload milestone instead of a wall of sleeps:

```rust
async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
    let target = ctx.process_ips()[0].clone();
    ctx.crash(&target)?;                       // crash now, hold down

    // Wait until the workload has executed 100 operations.
    while ctx.state().get::<u64>("ops").unwrap_or(0) < 100 {
        ctx.time().sleep(Duration::from_millis(20)).await
            .map_err(|e| SimulationError::InvalidState(e.to_string()))?;
        if ctx.chaos_shutdown().is_cancelled() { return Ok(()); }
    }

    ctx.restart(&target)?;                     // explicit recovery
    Ok(())
}
```

Everything the script reads is per-iteration simulated state, so the crash target, the milestone and the restart point replay from the seed. `crates/moonpool-sim/tests/scripted_faults.rs` is the full version: it checks that a held-down process runs no application work until the restart.

`restart` on a process that is still running is an **in-place restart**. The old boot is killed like a force kill: its task, the tasks it spawned and its connections, which peers see aborted. Storage is left alone, as when a process exits on its own. The fresh boot starts one scheduler tick later, after the executor has dropped the old one, so two boots of one process never overlap. The kill is recorded in the fault timeline as a `process_force_kill` with cause `restart_in_place`.

## Publishing Vetoes and Reading Landings

An injector is the natural home for the harness's budgets. At the start of the window it can publish a `HintVeto` or an `OutageVeto` in `ctx.state()`, and the built-in injectors ask them before they strike. It can also wait on what the built-ins publish: an `OutageLanded` under `OUTAGE_STATE_KEY` marks the instant every memory of a group is gone, the right moment to plant latent disk damage. [hint!](29-hints.md) and [Correlated Outages](30-outages.md) show both vetoes.

## Rules for an Injector

1. Draw randomness only from `ctx.random()` or the `sim_random_*` functions. Never from `rand::thread_rng()` or the wall clock.
2. Sleep only through `ctx.time()`. A real sleep stalls the executor.
3. Check `ctx.chaos_shutdown()` in every loop and after every sleep, then return. An injector that keeps injecting after the window keeps the quiet tail from being quiet.
4. Keep the draw count independent of the outcome. A resample loop makes the schedule depend on what it found, and an exploration recipe keyed on the draw count stops replaying.
5. Do not keep state across seeds. The factory builds a fresh injector for each one. Share observations through an `Arc` the factory captures.
6. Record what the script did with `assert_reachable!` and `assert_sometimes!`, so the report shows the scenario ran. An injector that was never reached is a scenario that was never tested.

`crates/moonpool-rpc-sim` is a larger example: each campaign ships an injector that scripts DNS changes, lost replies, one-way partitions and crashes against a named server group, and coordinates with its workload through `state()`.
