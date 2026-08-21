# Crate Map

<!-- toc -->

Moonpool is organized as a workspace of nine crates. The dependency graph is deliberately layered: lower crates know nothing about higher ones, and the leaf crate (`moonpool-explorer`) has no moonpool dependencies at all.

## Dependency Diagram

```text
                            moonpool
                         (facade crate)
              /          /        |         \
             /          /         |          \
  moonpool-hyper  moonpool-   moonpool-sim   moonpool-core
  (hyper 1.x,     transport   (simulation)  (provider traits)
   feature-gated) (peer, wire,      |
        |          RPC)             |
        |             |   \         |
        |             |    moonpool-transport-derive
        |             |    (proc macros)
        |             |             |
        |             |             v
        |             |       moonpool-explorer
        |             |    (fork-based exploration)
        |             |             |
        |             |             v
        |             |           libc
        |             |
        +-------------+-------------> moonpool-core

  moonpool-sim-examples (example simulation binaries)
  xtask (cargo automation, not a library dependency)
```

## Crate Details

### moonpool

**Role**: Facade crate. Re-exports everything from the lower crates so users only need one dependency.

**Dependencies**: moonpool-core, moonpool-sim, moonpool-transport, moonpool-hyper (optional, feature `hyper`)

**Key types**: Re-exports all types from moonpool-core, moonpool-sim, and moonpool-transport at the root. With the `hyper` feature, moonpool-hyper is exposed as the namespaced `moonpool::hyper` module rather than a fourth glob.

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
- `SimWorld` -- the simulated world containing network, time, storage, and event queue
- `SimulationBuilder` -- builder pattern for configuring experiments
- `SimContext` -- per-workload context providing access to providers and topology
- `NetworkConfiguration` / `ChaosConfiguration` -- network chaos parameters
- `Process` -- trait for system-under-test server processes
- `Workload` -- trait for test driver workloads
- `Attrition` -- automatic process reboot configuration
- `FaultInjector` / `FaultContext` -- custom fault injection during chaos phase
- `IterationControl` -- how many iterations to run
- `SimulationReport` -- results, metrics, and assertion data
- `Invariant` -- trait for cross-system property validation

**Assertion macros** (15 total): `assert_always!`, `assert_sometimes!`, `assert_reachable!`, `assert_unreachable!`, `assert_always_greater_than!`, `assert_sometimes_each!`, and more. See the [Assertion Reference](./01-assertion-reference.md) for the complete list.

---

### moonpool-transport

**Role**: Peer connections, wire format, FlowTransport-style networking, and RPC. Modeled after FoundationDB's FlowTransport.

**Dependencies**: moonpool-core, moonpool-transport-derive, async-trait, crc32c, futures, serde, serde_json, thiserror, tokio, tokio-util, tracing

**Key types**:
- `Peer` -- manages a connection to a remote endpoint with automatic reconnection
- `PeerConfig` -- reconnection delays, queue size, connection timeout
- `MonitorConfig` -- ping-based connection health monitoring
- `NetTransport` -- central coordinator managing peers and packet dispatch
- `EndpointMap` -- hybrid token routing (O(1) well-known, O(log n) dynamic)
- `FailureMonitor` / `FailureStatus` -- reactive address/endpoint failure tracking
- `ReplyPromise` -- server-side response promise (auto-sends `BrokenPromise` on Drop)
- `ReplyFuture` -- client-side response future (auto-closes queue on Drop)
- `ReplyError` -- error enum including `MaybeDelivered`, `Timeout`, `BrokenPromise`
- `RequestStream` -- server-side typed request receiver with bound transport (`recv()`)
- `RequestEnvelope` -- request + reply_to endpoint for bidirectional RPC
- `MessagingError` -- transport-level error type
- Delivery modes: `send`, `try_get_reply`, `get_reply`, `get_reply_unless_failed_for`
- Smoothing: `Smoother` (EMA utility from FDB's Smoother.h)

**Proc macros** (from moonpool-transport-derive):
- `#[service]` -- generates service trait, server, client, and bound client types

---

### moonpool-transport-derive

**Role**: Procedural macros for RPC service definitions.

**Dependencies**: proc-macro2, quote, syn (compile-time only, no runtime deps)

**Provides**:
- `#[service]` -- derive macro for RPC service definitions

This is a proc-macro crate and cannot export regular types or functions.

---

### moonpool-hyper

**Role**: hyper 1.x integration over the provider traits, so an HTTP/2 stack (tonic gRPC, axum, plain hyper) runs unchanged in production and in simulation. Optional everywhere: the facade gates it behind the `hyper` feature, and the lean production tree contains no hyper at all.

**Dependencies**: moonpool-core, futures, hyper, pin-project-lite, thiserror, tower-service, tracing

**Key types**:
- `HyperExecutor` -- a `hyper::rt::Executor` over `TaskProvider`
- `HyperTimer` -- a `hyper::rt::Timer` over `TimeProvider`, so h2 keepalive runs on provider time
- `HyperIo` -- hyper's `rt::Read` and `rt::Write` over any futures-io stream, with opt-in vectored writes
- `TowerToHyperService` -- adapts a tower service to hyper's `Service`
- `ReconnectingChannel` / `ChannelConfig` -- the `tonic::transport::Channel` role: lazy connect, jitter-free backoff, one multiplexed h2 connection
- `H2Server` / `H2ServerConfig` -- per-connection serve helper with graceful drain
- `ChannelError` -- typed, `Clone` channel failures
- `KeepAlive` -- h2 PING settings shared by both sides

**Features**: `client` and `server`, both on by default; the featureless build is just the adapters.

---

### moonpool-explorer

**Role**: Frontier-based exploration controller: recipes, discovery journals, bounded worker pool, exemplar store. A **leaf crate** with zero moonpool knowledge -- communicates with the simulation through the assertion accounting hooks and an RNG call-count function pointer.

**Dependencies**: libc (only dependency)

**Key types**:
- `Explorer` -- the controller: frontier, exemplars, novelty, worker pool
- `ExplorationConfig` -- workers, run budget, branching factor, frontier/depth caps
- `ExploreJob` / `Recipe` -- a replayable timeline (RNG breakpoint list)
- `DiscoveryEvent` -- one journaled discovery (kind, semantic state id, RNG call count)
- `ExplorationStats` -- per-seed exploration statistics
- Re-exports from `moonpool-assertions`: `AssertionSlot`, `AssertKind`, `AssertCmp`, `EachBucket`, `DiscoveryKind`, the `assertion_*` accounting functions

**Key functions**:
- `Explorer::new()` / `begin_seed()` / `observe_root_run()` / `explore()` -- the controller lifecycle
- `init_assertions()` / `cleanup_assertions()` -- shared assertion-region lifecycle
- `set_rng_count_hook()` -- connect to the simulation's RNG call counter
- `explorer_is_child()` -- whether the current process is a forked worker
- `format_timeline()` / `parse_timeline()` -- recipe serialization
- `sancov_edges_covered()`, `sancov_edge_count()`, `sancov_is_available()` -- sanitizer coverage integration

---

### moonpool-sim-examples

**Role**: Example simulation binaries demonstrating exploration features.

**Dependencies**: moonpool-sim

**Binaries**:
- `sim-maze-explore` -- frontier exploration on maze workload
- `sim-dungeon-explore` -- frontier exploration on dungeon workload

**Not a library dependency** -- contains only binary targets for demonstration and testing of the exploration subsystem.

---

### xtask

**Role**: Cargo xtask automation for running simulation binaries.

**Not a library dependency** -- invoked via `cargo xtask`.

**Commands**:
- `cargo xtask sim list` -- list all simulation binaries
- `cargo xtask sim run <filter>` -- run simulation binaries matching a filter
- `cargo xtask sim run-all` -- run all simulation binaries
