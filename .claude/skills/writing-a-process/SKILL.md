---
name: writing-a-process
description: Implement a moonpool Process (the system under test) - the async_trait Process trait, the per-boot factory, SimContext accessors, graceful shutdown via ctx.shutdown(), process groups and tags, and what survives a reboot. Use when creating a server or node for simulation, wiring a Process factory into SimulationBuilder, handling RebootKind, or when code inside a Process reaches for tokio directly.
---

# Writing a Process

A `Process` is the server. Moonpool kills it and calls the factory again on
every reboot, so the only state that survives is what went through
`ctx.storage()`. Everything in the struct's fields is gone on restart, and a
test that "works" because a field survived is testing the wrong system.

## The trait

```rust
use async_trait::async_trait;
use moonpool_sim::{Process, SimContext, SimulationResult};

pub struct MyServer { /* fresh per boot */ }

#[async_trait]                                    // no (?Send): futures must be Send
impl Process for MyServer {
    fn name(&self) -> &str { "my-server" }       // also the process *group* name

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        loop {
            moonpool_sim::select! {                // never tokio::select!
                () = ctx.shutdown().cancelled() => return Ok(()),
                accepted = listener.accept() => { let (stream, _) = accepted?; /* serve */ }
            }
        }
    }
}
```

`Process: Send + Sync + 'static`, and the executor is single-threaded, so
`Arc<RwLock<_>>`, `DashMap` and atomics are fine; `Rc`/`RefCell` across an
`.await` are not, and there is no `LocalSet`.

## What `SimContext` gives you

`ctx.network()`, `ctx.time()`, `ctx.task()`, `ctx.random()`, `ctx.storage()`,
`ctx.block_devices()` are the five providers plus block devices; `ctx.my_ip()`
is the bind address; `ctx.shutdown()` is the `CancellationToken` a graceful
reboot cancels; `ctx.topology()` answers `ips_in_group("other-role")`,
`my_group()` and `groups()`; `ctx.state()` is the shared `StateHandle`
(`publish` / `get`) a workload, invariant or fault injector reads. Never call
`tokio::*` for any of these (`/using-providers` has the mapping).

## Reboots

| `RebootKind` | What happens |
|---|---|
| `Graceful` | `ctx.shutdown()` is cancelled, a grace period passes, then force kill, then the factory runs again |
| `Crash` | immediate abort, factory runs again; storage keeps torn writes |
| `CrashAndWipe` | as `Crash`, and the process's simulated storage is wiped |

Check `ctx.shutdown().is_cancelled()` (or select on `.cancelled()`) in every
long loop; a process that ignores the token turns each graceful reboot into a
force kill and hides the shutdown-path bugs the graceful kind exists to find.

Attrition is enabled with `Chaos::Attrition { config: Attrition { .. }, mode }`
and needs `.chaos_duration(..)` on the builder; `Attrition` has no `Default`,
so name all eight fields. `AttritionVictims::group("name")` or
`::tagged(key, value)` scopes both the draw and `max_dead` to one pool.

## Groups and tags

Each `.processes(count, factory)` call registers one group named by
`Process::name()`; group *g* gets `10.0.g.{1..=count}` and its own per-seed
count when `count` is a range. Roles therefore come from **groups**, not from
parsing IPs. `.tags(&[("role", &["leader", "follower"])])?` distributes tags
round-robin over the last group and returns `Result`.

## Boot rules that find bugs

- Load persisted state **before** binding the listener, or a request can
  arrive at an uninitialized server; that ordering is a real bug class.
- Publish to `ctx.state()` only once the value is valid.
- Put a `buggify!()` at every optional-work or error-handling boundary in the
  boot path (`/using-buggify`), and an `assert_sometimes!` on the recovery
  outcomes you want proven reachable (`/using-assertions`).

Book: `book/src/part2-foundations/09-process.md`,
`part3-building/02-defining-process.md`, `part3-building/09-attrition.md`.
Reference implementations: `crates/moonpool-sim-examples/src/tonic_grpc.rs`
(a real gRPC server under attrition), `crates/moonpool-sim/tests/process_groups.rs`.
