# Crate Map

<!-- toc -->

Moonpool is organized as a workspace of eight crates. The dependency graph is deliberately layered: lower crates know nothing about higher ones, and the leaf crate (`moonpool-explorer`) has no moonpool dependencies at all.

## Dependency Diagram

```text
                        moonpool
                  (facade + virtual actors)
                   /         |         \
                  /          |          \
  moonpool-transport    moonpool-sim    moonpool-core
  (peer, wire, RPC)     (simulation)   (provider traits)
        |      \            |
        |       \           |
        |   moonpool-       |
        |   transport-      |
        |   derive          |
        |   (proc macros)   |
        |                   |
        +---> moonpool-core |
        +---> moonpool-sim  |
                   |        |
                   v        v
             moonpool-explorer
          (fork-based exploration)
                   |
                   v
                 libc

  moonpool-sim-examples (example simulation binaries)
  xtask (cargo automation, not a library dependency)
```

## Crate Details

### moonpool

**Role**: Facade crate and virtual actors. Re-exports everything from the lower crates so users only need one dependency.

**Dependencies**: moonpool-core, moonpool-sim, moonpool-transport, async-trait, serde, serde_json, tokio, tracing

**Key types**:
- `ActorHost` -- hosts and routes messages to virtual actors
- `ActorHandler` -- trait for actor message handling with lifecycle hooks
- `ActorContext` -- per-dispatch context with providers and optional state store
- `PersistentState<T>` -- typed wrapper for actor state persistence
- Re-exports all types from moonpool-core, moonpool-sim, and moonpool-transport

---

### moonpool-core

**Role**: Provider traits and core type definitions. Defines the abstraction boundary between real and simulated runtimes.

**Dependencies**: async-trait, rand, serde, serde_json, thiserror, tokio, tracing

**Key traits**:
- `TimeProvider` -- `sleep()`, `timeout()`, clock access
- `TaskProvider` -- `spawn_task()` for local task spawning
- `NetworkProvider` -- TCP listener and stream creation
- `RandomProvider` -- deterministic random number generation
- `StorageProvider` -- file I/O with simulation support

**Key types**:
- `Endpoint` -- `(IpAddr, Token)` pair identifying a connection endpoint
- `UID` -- unique identifier type
- `NetworkAddress` -- parsed network address
- `WellKnownToken` -- reserved token namespace for framework services
- `Providers` -- bundle of all provider traits
- `SimulationError` / `SimulationResult` -- error types

---

### moonpool-sim

**Role**: Simulation runtime, chaos testing, buggify system, and assertion macros. The core simulation engine that drives deterministic testing.

**Dependencies**: moonpool-core, moonpool-explorer, async-trait, crc32c, futures, rand, rand_chacha, serde, serde_json, thiserror, tokio, tokio-util, tracing

**Key types**:
- `SimWorld` -- the simulated world containing network, time, and event queue
- `SimulationBuilder` -- builder pattern for configuring experiments
- `SimContext` -- per-workload context providing access to providers and topology
- `NetworkConfiguration` / `ChaosConfiguration` -- network chaos parameters
- `Process` -- trait for system-under-test server processes
- `Workload` -- trait for test driver workloads
- `Attrition` -- automatic process reboot configuration
- `PhaseConfig` -- chaos/recovery phase durations
- `IterationControl` -- how many iterations to run
- `SimulationReport` -- results, metrics, and assertion data
- `Invariant` -- trait for cross-actor property validation

**Assertion macros** (15 total): `assert_always!`, `assert_sometimes!`, `assert_reachable!`, `assert_unreachable!`, `assert_always_greater_than!`, `assert_sometimes_each!`, and more. See the [Assertion Reference](./assertion-reference.md) for the complete list.

---

### moonpool-transport

**Role**: Peer connections, wire format, FlowTransport-style networking, and RPC. Modeled after FoundationDB's FlowTransport.

**Dependencies**: moonpool-core, moonpool-sim, moonpool-transport-derive, async-trait, crc32c, futures, serde, serde_json, thiserror, tokio, tokio-util, tracing

**Key types**:
- `Peer` -- manages a connection to a remote endpoint with automatic reconnection
- `PeerConfig` -- reconnection delays, queue size, connection timeout
- `MonitorConfig` -- ping-based connection health monitoring
- `PeerReceiver` -- receives messages from a peer
- `WireMessage` / `WireHeader` -- on-the-wire message format with CRC32C integrity
- RPC types: `ServiceRegistry`, `RpcClient`, `RpcServer`, `ReplyPromise`
- `MessagingError` -- transport-level error type

**Proc macros** (from moonpool-transport-derive):
- `#[service]` -- generates service trait, request/response enums, and routing
- `#[actor_impl]` -- generates actor message dispatch boilerplate

---

### moonpool-transport-derive

**Role**: Procedural macros for service and actor definitions.

**Dependencies**: proc-macro2, quote, syn (compile-time only, no runtime deps)

**Provides**:
- `#[service]` -- derive macro for RPC service definitions
- `#[actor_impl]` -- derive macro for actor message dispatch

This is a proc-macro crate and cannot export regular types or functions.

---

### moonpool-explorer

**Role**: Fork-based multiverse exploration, coverage tracking, and energy budgets. A **leaf crate** with zero moonpool knowledge -- communicates with the simulation only through RNG function pointers.

**Dependencies**: libc (only dependency)

**Key types**:
- `ExplorationConfig` -- max depth, energy, timelines per split, adaptive config
- `AdaptiveConfig` -- batch size, min/max timelines, per-mark energy
- `Parallelism` -- multi-core exploration variants (MaxCores, HalfCores, Cores, MaxCoresMinus)
- `AssertionSlot` / `AssertionSlotSnapshot` -- shared-memory assertion tracking (128 slots)
- `AssertKind` -- Always, AlwaysOrUnreachable, Sometimes, Reachable, Unreachable, NumericAlways, NumericSometimes, BooleanSometimesAll
- `AssertCmp` -- Gt, Ge, Lt, Le
- `EachBucket` -- per-value bucketed assertion tracking (256 buckets)
- `CoverageBitmap` / `ExploredMap` -- 8192-bit coverage bitmaps
- `EnergyBudget` -- 3-level energy system (global + per-mark + reallocation pool)
- `SharedStats` / `SharedRecipe` -- cross-process counters and bug replay data
- `ExplorationStats` -- snapshot of exploration progress

**Key functions**:
- `init()` / `cleanup()` -- lifecycle management
- `init_assertions()` / `cleanup_assertions()` -- assertion-only lifecycle
- `set_rng_hooks()` -- connect to simulation's RNG
- `assertion_bool()`, `assertion_numeric()`, `assertion_sometimes_all()`, `assertion_sometimes_each()` -- assertion recording
- `split_on_discovery()` -- fork the process at a splitpoint
- `exit_child()` -- terminate a forked child process
- `prepare_next_seed()` -- selective reset for multi-seed exploration
- `sancov_edges_covered()`, `sancov_edge_count()`, `sancov_is_available()` -- sanitizer coverage integration

---

### moonpool-sim-examples

**Role**: Example simulation binaries demonstrating exploration features.

**Dependencies**: moonpool-sim

**Binaries**:
- `sim-maze-explore` -- adaptive exploration on maze workload
- `sim-dungeon-explore` -- adaptive exploration on dungeon workload

**Not a library dependency** -- contains only binary targets for demonstration and testing of the exploration subsystem.

---

### xtask

**Role**: Cargo xtask automation for running simulation binaries.

**Not a library dependency** -- invoked via `cargo xtask`.

**Commands**:
- `cargo xtask sim list` -- list all simulation binaries
- `cargo xtask sim run <filter>` -- run simulation binaries matching a filter
- `cargo xtask sim run-all` -- run all simulation binaries
