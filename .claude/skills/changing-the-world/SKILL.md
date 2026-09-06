---
name: changing-the-world
description: Change moonpool's simulated world safely - SimWorld and the single Scheduler<Event> that owns logical time, the NetworkSimulation and StorageEngine components and what each owns, the effects/WakeBatch protocol back to the coordinator, the never-wake-under-the-lock rule, the Pending-then-event completion model, the one shared random stream in sim/rng.rs, and the FoundationDB-first reading rule (sim2.actor.cpp, Net2, docs/analysis/foundationdb). Use before editing anything under crates/moonpool-sim/src/{sim,network,storage,executor}, when a completion order or timing changes, or when a test needs to drive a provider future by hand.
---

# Changing the world

The simulated world is split by **ownership**, and every determinism bug in
this repository's history came from blurring one of the lines below or from a
draw taken outside the one stream. Read the FoundationDB material first:
`docs/analysis/foundationdb/layer-1-flow-runtime.md` before anything in
`actor.cpp`, `docs/analysis/foundationdb/simulation.md` for the sim2 model
(virtual time, the event queue, `Sim2Conn`, connection close, the
`IAsyncFile` decorator stack), then the source under `docs/references/foundationdb/`
(`sim2.actor.cpp`, `Net2.actor.cpp`, `Net2Packet.h`, `AsyncFileNonDurable`,
`AsyncFileChaos.h`). TigerBeetle's `packet_simulator.zig` and `storage.zig`
are the second reference. The `reference-reader` agent can do this pass for
you and report the pattern.

## Who owns what

| Component | Files | Owns |
|---|---|---|
| `SimWorld` | `sim/world.rs`, `sim/events.rs`, `sim/wakers.rs`, `sim/sleep.rs`, `sim/storage_ops.rs` | lifecycle and the one `Scheduler<Event>`: monotonic logical time (`now`), same-time FIFO sequence allocation, cancellation (`ScheduleId`), `step()`, `has_pending_events()`, `take_faults()` |
| `NetworkSimulation` | `network/sim/{engine,state,stream,delay,event,facade,provider,types}.rs`, `network/config.rs` | topology, connections, faults and their per-seed configuration, pending operations, results, network wakers. Bind, connect and accept are real delayed operations; established stream I/O is buffer-driven and may complete on the current poll |
| `StorageEngine` | `storage/sim/{engine,state,event}.rs`, `storage/{config,file,futures,memory,provider,events,error}.rs`, `storage/block/` | files, independent open handles, disk configs and degradation episodes, exact pending/completed operations, faults, storage wakers. Read, write, sync and set_len return `Pending` and complete through a scheduled `StorageEvent` |
| executor | `executor/{mod,join}.rs` | the single-threaded seeded task scheduler; `Send + 'static` futures, no `LocalSet` |
| the stream | `sim/rng.rs` | every draw, counted; the determinism canary |

Components return **ordered scheduling and cancellation effects** and
`WakeBatch`es to the coordinator; the coordinator applies them and wakes
after the lock is released. Two rules are absolute: never move component
state or wakers back into `SimInner`, and never invoke a waker while holding
the world lock (a woken task may re-enter the world on the same thread).

## Randomness

There is no private framework RNG. A fault decision, a delay, the task the
executor polls next, a `select!` branch offset, a swarm mask, a buggify
activation and every `RandomProvider` draw come from `SIM_RNG`
(`sim_random*`), so the same seed replays bit for bit and adding a draw
anywhere shifts everything after it. When you add a decision, draw it from
the stream, draw it a **fixed** number of times regardless of outcome
(`sim_random_bool` is always one draw; a resample loop is not), and expect the
canary tests (`tests/determinism_canary.rs`, `tests/determinism.rs`) to be the
first thing that tells you if you got it wrong. `rand::thread_rng`,
`HashMap` iteration, `Instant::now` and statics that outlive
`reset_per_iteration_state` are the four ways a draw escapes.

## Time and events

Logical time only moves inside `Scheduler`; nothing else reads a clock.
Same-time events run in scheduling order (the sequence number), so reordering
two `schedule_at` calls is a behaviour change that every seed will show.
Infrastructure-only events (partition expiry) let a run terminate early
(`Event::is_infrastructure_event`); a new event kind must say which side of
that line it is on. Simulator-injected faults are recorded as
`SimFaultRecord`s and surface in the timeline as `sim_fault` events.

## Testing a change here

Provider-level tests drive the future and the world together on the
executor:

```rust
let mut executor = moonpool_sim::executor::Executor::new(seed);
executor.block_on(async move {
    let provider = sim.storage_provider(ip);
    drive(&mut sim, async move { /* open, write_all, sync_all */ }).await
})?;
```

with the `drive` helper from the root `AGENTS.md` (`poll` → on `Pending`,
`sim.step()` while `has_pending_events()`). Subsystem tests live in
`tests/{network,storage,sim,executor}.rs` plus their directories;
`tests/flow_control.rs`, `raw_network_characterization.rs`,
`h2_partition_integrity.rs` pin the network's observable behaviour. After a
world change run the subsystem file, then `cargo xtask sim run-all` (every
example is a scheduler regression surface), then the portability builds
(`/validate`): `sim/`, `network/`, `storage/` must stay wasm-clean with
`--no-default-features`.

Book: `book/src/part2-foundations/01-determinism.md`, `02-single-core.md`,
`11-executor.md`, `part4-networking/01-simulating-network.md`.
