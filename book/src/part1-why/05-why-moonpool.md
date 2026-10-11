# Why Moonpool Exists

<!-- toc -->

After ten years of operating distributed systems, one pattern keeps repeating: the bugs that hurt most are the ones nobody imagined.

You build a system. You test the scenarios you can think of. You deploy. Then a partition overlaps with a retry storm during a rolling upgrade, and the system enters a state nobody on the team anticipated. The code was not wrong. It was never tested against that combination of events.

The combinatorial problem from [The Case for Simulation](./01-case-for-simulation.md) makes this inevitable. You cannot test what you do not know. Manual tests give regression coverage, not absence of bugs. That realization led to moonpool: the testing toolbox I wish I had when I started building distributed systems.

## What already existed

The answer was scattered across several projects and two decades of work. **FoundationDB** showed that a whole cluster can run on one thread with one seeded random stream, and that fault injection on top of that finds more bugs in a night than production finds in a year. **TigerBeetle** took the same idea to the disk, with faults drawn from real hardware failure modes. **Antithesis** turned assertions into a declarative SDK and used them to guide a search that forks at every discovery. [A Brief History](./04-history.md) tells that story.

Nobody had put these pieces together as a Rust library you add with one line in `Cargo.toml`. That is the gap moonpool fills.

## Where moonpool sits

Moonpool is a **library-level** simulation framework. There is no hypervisor, no custom virtual machine, no special runtime. You add a dependency, write your code against six provider traits, and your system becomes simulatable. Simulations run as ordinary `cargo test` invocations in your CI pipeline.

This is a deliberate trade. Antithesis's hypervisor simulates any software without modification. Moonpool asks that your I/O go through the providers, which means designing for simulation from the start or refactoring existing code ([Migrating Existing Code to Providers](../part4-integration/06-migrating-existing-code.md)). In exchange you get zero-cost abstractions in production, full control over fault injection, and no service to call.

## Same code in production and simulation

One rule shapes every API in moonpool: **the code you test is the code you ship**. Shipped code is never wrapped in a `Sim*` variant. There is no `SimMachine`, no `SimDisk`, no test-only hook. Code depends on provider traits. `TokioProviders` backs them in production, and the simulator's providers back them in tests.

When shipped code knows that an instant is interesting, it names the moment with `hint!("label").await`. In production that is a no-op. In simulation the simulator decides whether to strike there and how, under the seed's chaos, its swarm masks and its budgets. The code's own rare-but-legal choices use `buggify!`, `buggify_with_prob!` or `buggify_named!`, inert outside a simulation. The rule and its consequences are in [Same Code in Production and Simulation](../part2-foundations/12-same-code.md).

## What moonpool offers today

Every feature below has a chapter. This is the map.

**Six provider traits.** `TimeProvider`, `TaskProvider`, `NetworkProvider`, `RandomProvider`, `StorageProvider` and `SystemProvider` in `moonpool-core`, bundled by the `Providers` trait. `TokioProviders` is the production bundle. The traits are `Send + Sync`, so your code keeps its `Arc<RwLock<_>>` and `DashMap`. See [The Provider Pattern](../part2-foundations/04-provider-pattern.md) and [The Six Providers](../part2-foundations/07-provider-traits.md).

**A deterministic executor.** One OS thread, seeded task scheduling, logical time that jumps when every task is blocked. `moonpool_sim::select!` replaces `tokio::select!` so branch order comes from the seed, not from process entropy. See [The Deterministic Executor](../part2-foundations/11-executor.md).

**Process and Workload.** A `Process` is the system under test. The simulator creates a fresh one on every boot. A `Workload` is the test driver. It survives reboots, drives requests and checks results. See [System Under Test vs Test Driver](../part2-foundations/08-process-workload.md).

**The `SimulationBuilder`.** `.processes()` and `.cluster()` register process groups on their own subnets, `.tags()` spreads roles, `.enable_chaos()` picks surfaces, `.chaos_duration()` bounds the window, `.invariant()` and `.metrics_factory()` add observers, `.until_coverage_stable()` and `.set_debug_seeds()` control iterations, and `.run()` returns a `SimulationReport`. See [Configuring the SimulationBuilder](../part3-building/04-simulation-builder.md) and [Process Groups, Tags and Topology](../part3-building/27-process-groups.md).

**Chaos surfaces.** `Chaos::Network`, `Chaos::Storage`, `Chaos::Attrition`, `Chaos::Outage`, `Chaos::BuggifyKnobs`, `Chaos::Cpu` and `Chaos::GrayFailure`, each with a `ChaosMode`. `ChaosMode::Random` turns everything on. `ChaosMode::Swarm` enables a random subset per seed. See [Chaos in Moonpool](../part3-building/07-chaos.md).

**Buggify and hints.** `buggify!` sites are activated once per run and fire with a probability. `buggify_named!` with `set_activation` builds per-seed scenarios. `buggify_knob!` pushes a configuration knob to an extreme. `hint!` names a moment and leaves the fault to the simulator, with `HintVeto` as a harness budget. See [Buggify](../part3-building/08-buggify.md) and [hint!](../part3-building/29-hints.md).

**Swarm testing.** One workload, many behaviors. `network_fault_mask`, `storage_fault_mask` and `swarm_operations` give each seed its own subset of fault families and operations, so a seed with few faults reaches deep states and a seed with many faults stresses recovery. See [Swarm Testing](../part3-building/28-swarm-testing.md).

**Attrition, outages and self-crashes.** `Attrition` reboots processes gracefully or by crash under a regime with a `max_dead` budget. `Chaos::Outage` takes a whole group down at one instant, and `OutageVeto` lets the harness defer it. `SimContext::crash_self` lets a workload crash its own process and get a `SelfCrash` handle. See [Attrition](../part3-building/09-attrition.md) and [Correlated Outages and Self-Crashes](../part3-building/30-outages.md).

**Network faults.** Connect delays, random close, bit flips, partial writes, packet loss and partitions, all on real byte streams through `NetworkProvider`. See [Network Faults](../part3-building/10-network-faults.md) and [Simulating the Network](../part4-networking/01-simulating-network.md).

**Storage faults.** One `StorageProvider`, one `StorageFile`, `OpenOptions::direct_io` for database-shaped needs and `BlockFile` for block arithmetic. Faults follow TigerBeetle and FoundationDB: corruption, torn and misdirected writes, phantom writes, sync failures, stalls, and the barrier-bounded crash model where every sector written since the last sync resolves on its own. A lost-synced-write oracle fails any run where a sector a sync called durable changes across a crash. See [Storage Faults](../part3-building/11-storage-faults.md).

**Aimed storage faults.** Injection scoped to an owner and a byte range, `LayoutRegion` as a format-neutral map, and `ReplicatedFaults` patterns (`Minority`, `Striped`, `Rolling`) that damage replicas while a clean copy survives somewhere. See [Aiming Storage Faults](../part3-building/33-aimed-faults.md).

**CPU time and gray failures.** `Chaos::Cpu` charges every poll to the process's one core. `cpu_charge` lets shipped code declare a cost. `Chaos::GrayFailure` keeps a machine alive and slow, and `SimContext::slowness` tells a workload about it. `SystemProvider` exposes the counters your code reads to detect it. See [CPU Time and Gray Failures](../part3-building/26-cpu-and-gray-failure.md).

**Custom fault injectors.** `fault_factory` builds a fresh `FaultInjector` per timeline, with a `FaultContext` that can crash, restart, read `durable_bytes` and read workload state. See [Custom Fault Injectors](../part3-building/31-custom-faults.md).

**Fifteen assertion macros.** `assert_always!`, `assert_sometimes!`, `assert_reachable!`, `assert_unreachable!`, `assert_always_or_unreachable!`, eight numeric forms, `assert_sometimes_all!` and `assert_sometimes_each!`. See [Assertions: Finding Bugs](../part3-building/12-assertions.md) and the [Assertion Reference](../appendix/01-assertion-reference.md).

**Events and invariants.** Correctness facts are plain `tracing` events. `SimulationLayer` captures them into a timeline, and an `Invariant` reads them through `TraceQuery` after every step. The same events feed production observability. See [Events and Invariants](../part3-building/17-events-and-invariants.md).

**Metrics.** `moonpool-prometheus` scrapes the counters, gauges and histograms your code already keeps into the report, per node and across seeds. See [Application Metrics](../part3-building/25-metrics.md).

**Debugging.** A failing seed is a number. `set_debug_seeds` with `set_iterations(1)` replays it, and the event trace reads backwards from the first violation. See [Debugging a Failing Seed](../part3-building/20-debugging.md).

**HTTP and gRPC.** `moonpool-hyper` supplies `HyperExecutor`, `HyperTimer` and `HyperIo`, so axum, tonic and plain hyper run unchanged on the providers. See [The hyper Stack](../part4-integration/08-hyper-stack.md) and [Wiring a Web Service](../part4-integration/03-wiring-a-web-service.md).

**Typed RPC.** `moonpool-rpc` is FoundationDB's `fdbrpc` without its wire format: interfaces as data, delivery modes, streams, per-endpoint `EndpointQueue` admission, buggify sites inside the runtime, and `derive`, `tls` and `jwt` features. `moonpool-rpc-sim` runs six campaigns in CI. See [Typed RPC with moonpool-rpc](../part4-networking/02-rpc.md) and [Admission, Security and Derive](../part4-networking/04-rpc-operations.md).

**Production and the browser.** The lean build `default-features = false, features = ["tokio"]` keeps the simulator out of your release binary. The simulator itself compiles to `wasm32-unknown-unknown`, and the book embeds a browser demo. See [Using Providers in Production](../part4-integration/05-production.md) and [Simulation in the Browser](../part4-integration/07-wasm.md).

**Multiverse exploration.** `moonpool-explorer` forks the process at every discovery, records a replay recipe of `(rng_call_count, seed)` breakpoints, and schedules bounded continuations through a pool of short-lived workers. Coverage comes from sancov edges under `cargo xtask sim run`. See [Multiverse Exploration](../part5-building-on-top/01-exploration.md).

**A crash-aware journal.** `moonpool-journal` implements CLSTORE over the providers: positions in any order, a crash told apart from corruption, damage named for repair from peers, and `hint!` inside every commit. See [A Crash-Aware Journal](../part5-building-on-top/08-journal.md).

**Tooling.** `cargo xtask sim list | run | run-all` runs the simulation binaries, and `moonpool-calibrate` measures a real host to produce latency constants. See [Running Simulation Binaries with xtask](../appendix/08-xtask.md) and [Calibrating Against a Real Machine](../appendix/07-calibration.md).

## What makes it different

Many frameworks stop at network-level fault injection. Moonpool adds three things.

First, **storage simulation** at the same fidelity as the network, aimed so that replicated data keeps a clean copy and the system has something to repair from.

Second, **fault sites in shipped code**. `hint!` lets the code name the moment and the simulator pick the fault, so the production binary carries its own test plan at no cost.

Third, **fork-based exploration**. The **sequential luck problem** (a bug that needs n unlikely events in a row has probability p^n) is the central bottleneck of simulation testing. Instead of hoping one timeline walks through every door, moonpool forks at each door and explores both sides.

Moonpool is not only a simulation engine. It is a simulation engine with a search strategy for the state spaces where bugs need a sequence of unlikely events to appear.
