# moonpool-sim

The simulation runtime: the deterministic executor, the simulated world, the
chaos surfaces, the assertion and buggify wiring, the runner and the
observability layer. Consumers reach all of it through `SimulationBuilder`
and `SimContext`.

## Ownership inside the world (do not blur these)

- `sim/` — `SimWorld` coordinates lifecycle and a single global
  `Scheduler<Event>`; the scheduler alone owns monotonic logical time,
  same-time FIFO sequence allocation and cancellation. `sim/rng.rs` is the
  **one random stream**: every draw the framework or the code under test
  makes goes through `SIM_RNG`, is counted (`rng_call_count`), and carries the
  determinism canary. There is no private framework RNG; adding a draw
  anywhere shifts every seed, which is intended.
- `network/` — `NetworkSimulation` owns topology, connections, faults, pending
  operations, results and network wakers. Bind, connect and accept are real
  delayed operations; established stream I/O is buffer-driven and may
  complete on the current poll.
- `storage/` — `StorageEngine` owns files, independent open handles, disk
  configs and degradation episodes, exact pending/completed operations, faults
  and storage wakers. Read/write/sync/set_len return `Pending` and complete
  through scheduled `StorageEvent`s. There is exactly **one** simulated file
  implementation (`storage/image.rs`): every API — stream, positioned, block —
  lands on the same bytes, and a write through one is observable through the
  others. `storage/faults.rs` is the fault vocabulary, addressed by file and
  flat sector offset. Do not add a second byte store for a new API.
- Components return ordered scheduling/cancellation effects and `WakeBatch`es
  to the coordinator. Never move component state or wakers back into
  `SimInner`, and never invoke a waker while holding the world lock.
- `executor/` — the single-threaded seeded executor. Futures are
  `Send + 'static`; no `LocalSet`, no `spawn_local`.
- `runner/` — `SimulationBuilder`, `Process`/`Workload`/`FaultInjector`,
  topology and groups, attrition, iteration control, the report.
- `chaos/` — assertion macros (accounting in `moonpool-assertions`), the
  buggify wiring (`buggify_init(0.5)` per run; the macros themselves are in
  `moonpool-buggify`, `buggify_knob!` stays here), `StateHandle`.
- `observability/` — the tracing layer that captures INFO+ constant-message
  events inside process/workload spans into the timeline, `TraceQuery`,
  `Invariant`.

## Features

`default = ["tokio-providers", "exploration"]`. With `--no-default-features`
the crate compiles to wasm and runs tasks on the moonpool executor, and
assertion accounting falls back to the in-process heap table, so every
coverage mode still works without the fork explorer. Keep that build green
(`cargo clippy -p moonpool-sim --no-default-features --all-targets`,
`cargo nextest run -p moonpool-sim --no-default-features`, the wasm check).

## Tests

Integration tests live in `tests/` with one file per subsystem plus a
directory of the same name for its cases (`network.rs` + `network/`,
`storage.rs` + `storage/`, `chaos.rs` + `chaos/`, ...). Low-level provider
tests drive the future and the world together on the executor (the `drive`
helper in the root `AGENTS.md`). `determinism.rs` / `determinism_canary.rs`
are the replay contracts; `leader_election.rs` is the canonical invariant
example; `swarm_op_alphabet.rs`, `process_groups.rs`, `scripted_faults.rs`,
`recovery_mode.rs` pin the builder features by name. A change to the draw
schedule (a new random decision, a reordered await in the runner) is expected
to change which seeds fail elsewhere; it must not change whether the same
seed replays identically.
