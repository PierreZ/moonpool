# Index

<!-- toc -->

This page is a map of the book. Each entry links to one chapter and says what it covers.

## Quick Start Routes

| Question | Start here |
|---|---|
| I am a coding agent and I must add a simulation harness | [Guide for AI Agents](./llms.md) |
| What is moonpool? | [The Case for Simulation](./part1-why/01-case-for-simulation.md), then [Why Moonpool Exists](./part1-why/05-why-moonpool.md) |
| What is new in moonpool? | [State of Moonpool](./foreword/01-state.md) |
| How do I write my first simulation? | [Your First Simulation](./part3-building/01-first-simulation.md) |
| How can one code base run in production and in simulation? | [Same Code in Production and Simulation](./part2-foundations/12-same-code.md) |
| How do providers work? | [The Provider Pattern](./part2-foundations/04-provider-pattern.md) |
| How do I add faults? | [Chaos in Moonpool](./part3-building/07-chaos.md) |
| How does my code mark a place where a fault is interesting? | [hint!: Naming Fault Sites](./part3-building/29-hints.md) and [Buggify](./part3-building/08-buggify.md) |
| How do I get a different fault mix on each seed? | [Swarm Testing: Masks and Budgets](./part3-building/28-swarm-testing.md) |
| How do I simulate slow machines and gray failures? | [CPU Time and Gray Failures](./part3-building/26-cpu-and-gray-failure.md) |
| How do I take down a whole rack? | [Correlated Outages and Self-Crashes](./part3-building/30-outages.md) |
| How do I use assertions? | [Assertions: Finding Bugs](./part3-building/12-assertions.md) |
| A seed failed. What now? | [Debugging a Failing Seed](./part3-building/20-debugging.md) |
| How do I test an existing app (axum, tonic, hyper)? | [Using moonpool-sim Standalone](./part4-integration/01-standalone-sim.md) |
| How do I ship this to production? | [Using Providers in Production](./part4-integration/05-production.md) |
| How do processes call each other? | [Typed RPC with moonpool-rpc](./part4-networking/02-rpc.md) |
| I need a durable log | [A Crash-Aware Journal](./part5-building-on-top/08-journal.md) |
| How does multiverse exploration work? | [Multiverse Exploration](./part5-building-on-top/01-exploration.md) |
| Which knobs exist and what are their defaults? | [Configuration Reference](./appendix/03-configuration.md) and [Fault Reference](./appendix/04-fault-reference.md) |

## Foreword

- [State of Moonpool](./foreword/01-state.md): the crates, what is stable, what is new, what is missing.

## Part I: Why Simulation Testing

- [The Case for Simulation](./part1-why/01-case-for-simulation.md): the gap between localhost and production, and why coverage numbers lie.
- [Prevention vs Discovery](./part1-why/02-prevention-vs-discovery.md): regression tests guard what we know; simulation finds what we do not know.
- [From Mocks to Simulation](./part1-why/03-mocks-to-simulation.md): why mocks and `#[cfg(test)]` fail at scale, and how traits replace them.
- [A Brief History](./part1-why/04-history.md): FoundationDB, Antithesis, TigerBeetle and what moonpool takes from each.
- [Why Moonpool Exists](./part1-why/05-why-moonpool.md): one short paragraph for each moonpool feature, with links.

## Part II: Foundations

- [Determinism as a Foundation](./part2-foundations/01-determinism.md): threads, I/O and randomness, and how moonpool removes each one.
- [The Single-Core Constraint](./part2-foundations/02-single-core.md): one thread gives one legal order; `Send`-bounded traits; no tokio runtime in a simulation.
- [Seed-Driven Reproducibility](./part2-foundations/03-seeds.md): one `u64` seed controls the run; RNG call counts; the opt-in determinism canary.
- [The Deterministic Executor](./part2-foundations/11-executor.md): seeded task scheduling, task owners, the CPU gate, kill-on-drop.
- [The Provider Pattern](./part2-foundations/04-provider-pattern.md): the `Providers` bundle; `TokioProviders` and `SimProviders`.
- [Quick Start: Swapping Implementations](./part2-foundations/05-providers-quickstart.md): one generic function, run on Tokio and in the simulator; the forbidden calls.
- [Deep Dive: Why Providers Exist](./part2-foundations/06-providers-deepdive.md): why not mocks, "never more permissive", static dispatch.
- [The Six Providers](./part2-foundations/07-provider-traits.md): Time, Task, Network, Random, Storage and System, plus `Resolver` and `select!`.
- [Same Code in Production and Simulation](./part2-foundations/12-same-code.md): no Sim* wrappers; `hint!`, `buggify!` and `cpu_charge` are no-ops in production; lean release builds.
- [System Under Test vs Test Driver](./part2-foundations/08-process-workload.md): why `Process` and `Workload` are two traits.
- [Process: Your Server](./part2-foundations/09-process.md): the factory, reboots, the context, `crash_self`.
- [Workload: Your Test Driver](./part2-foundations/10-workload.md): setup, run, check; the operation alphabet; `slowness` and system samples.

## Part III: Building Simulations

- [Your First Simulation](./part3-building/01-first-simulation.md): the plan and the full target program.
- [Defining a Process](./part3-building/02-defining-process.md): accept loop, `select!` on shutdown, ranged counts.
- [Writing a Workload](./part3-building/03-writing-workload.md): topology lookups, a reference model, why `run()` must finish.
- [Configuring the SimulationBuilder](./part3-building/04-simulation-builder.md): every builder method in one table, with links.
- [Running and Observing](./part3-building/05-running.md): nextest vs xtask, the report, stop conditions, stall guards.
- [Process Groups, Tags and Topology](./part3-building/27-process-groups.md): several roles, IP ranges, tags, `.cluster()` failure domains.
- [Chaos Testing vs Simulation](./part3-building/06-chaos-vs-simulation.md): two schools of breaking things, and where faults enter.
- [Chaos in Moonpool](./part3-building/07-chaos.md): every `Chaos` surface, `ChaosMode::Random` and `Swarm`, the chaos window.
- [Buggify: Fault Injection](./part3-building/08-buggify.md): `buggify!`, `buggify_with_prob!`, `buggify_named!`, `buggify_knob!`, two-phase activation.
- [hint!: Naming Fault Sites](./part3-building/29-hints.md): the code names a moment, the simulator decides; vetoes; the journal example.
- [Swarm Testing: Masks and Budgets](./part3-building/28-swarm-testing.md): fault masks, budgets, `swarm_operations`, one workload with many behaviours.
- [Attrition: Process Reboots](./part3-building/09-attrition.md): reboot kinds, `max_dead`, victims per group, failure domains.
- [Correlated Outages and Self-Crashes](./part3-building/30-outages.md): `Chaos::Outage`, `OutageVeto`, `SimContext::crash_self`.
- [Network Faults](./part3-building/10-network-faults.md): the nine families, partitions, black holes, network masks.
- [Storage Faults](./part3-building/11-storage-faults.md): storage layering, the barrier-bounded crash model, the lost-synced-write oracle.
- [Aiming Storage Faults](./part3-building/33-aimed-faults.md): `LayoutRegion`, fault focus, targeted injection, replicated fault patterns.
- [CPU Time and Gray Failures](./part3-building/26-cpu-and-gray-failure.md): the CPU gate, `cpu_charge`, `SystemProvider`, `Chaos::GrayFailure`, slow machines.
- [Custom Fault Injectors](./part3-building/31-custom-faults.md): `FaultInjector`, `fault_factory`, the `FaultContext` methods.
- [Assertions: Finding Bugs](./part3-building/12-assertions.md): the fifteen macros, where counts live, how assertions decide when to stop.
- [Invariants vs Discovery vs Guidance](./part3-building/13-assertion-concepts.md): three jobs, and what each kind does in the runner and the explorer.
- [Always and Sometimes](./part3-building/14-always-sometimes.md): the five boolean macros.
- [Numeric Assertions](./part3-building/15-numeric-assertions.md): the eight numeric macros and watermarks.
- [Compound Assertions](./part3-building/16-compound-assertions.md): `assert_sometimes_all!` and `assert_sometimes_each!`.
- [Events and Invariants](./part3-building/17-events-and-invariants.md): plain `tracing` events, the `Invariant` trait, the fault timeline.
- [Application Metrics](./part3-building/25-metrics.md): per-node Prometheus metrics, metric queries, a metastable failure.
- [Designing Workloads That Find Bugs](./part3-building/19-designing-workloads.md): operation alphabets, swarm, invariant patterns.
- [Debugging a Failing Seed](./part3-building/20-debugging.md): the workflow.
- [Reproducing with FixedCount](./part3-building/21-reproducing.md): pin the seed, turn up the logs.
- [Reading the Event Trace](./part3-building/22-event-trace.md): event kinds, faults in the timeline, RNG call counts.
- [Common Pitfalls](./part3-building/23-pitfalls.md): seventeen mistakes and their fixes.
- [Discovering Properties](./part3-building/24-discovering-properties.md): eight attention focuses to find what to assert.

## Part IV: Simulating Existing Applications

- [Using moonpool-sim Standalone](./part4-integration/01-standalone-sim.md): real HTTP over simulated TCP.
- [Where to Draw the Line](./part4-integration/02-mock-boundaries.md): fakes vs containers, per dependency.
- [Wiring a Web Service](./part4-integration/03-wiring-a-web-service.md): an axum service step by step.
- [The hyper Stack: gRPC and HTTP](./part4-integration/08-hyper-stack.md): `H2Server`, `ReconnectingChannel`, tonic.
- [What You're Testing (and What You're Not)](./part4-integration/04-scope-and-tradeoffs.md): the limits of the model.
- [Using Providers in Production](./part4-integration/05-production.md): `TokioProviders`, the lean dependency tree, inert fault sites.
- [Migrating Existing Code to Providers](./part4-integration/06-migrating-existing-code.md): the call mapping and the gotchas.
- [Simulation in the Browser](./part4-integration/07-wasm.md): the wasm build and the live demo.

## Part V: Raw Network Simulation

- [Simulating the Network](./part4-networking/01-simulating-network.md): TCP, not packets; who owns what.
- [Typed RPC with moonpool-rpc](./part4-networking/02-rpc.md): driver, endpoints, delivery modes, reply streams, failure monitor.
- [What moonpool-rpc Promises](./part4-networking/03-rpc-guarantees.md): the promises and the seven campaigns that check them.
- [Admission, Security and Derive](./part4-networking/04-rpc-operations.md): admission queues, buggify points, TLS, JWT, the derive macro.

## Part VI: Building on Top

- [Multiverse Exploration](./part5-building-on-top/01-exploration.md): fork at discovery.
- [The Exploration Problem](./part5-building-on-top/02-exploration-problem.md): the sequential luck problem.
- [The Frontier Controller](./part5-building-on-top/03-frontier-controller.md): recipes, the discovery journal, bug recipes.
- [Bounded Workers](./part5-building-on-top/04-bounded-workers.md): a fixed pool of forked workers.
- [Exemplars and Continuations](./part5-building-on-top/05-exemplars-continuations.md): semantic states and the under-explored frontier.
- [Multi-Seed Exploration](./part5-building-on-top/06-multi-seed.md): exploration across many seeds.
- [Exploring a Consensus Protocol](./part5-building-on-top/07-exploring-consensus.md): a worked procedure.
- [A Crash-Aware Journal](./part5-building-on-top/08-journal.md): moonpool-journal, its commit hints, and its tests under `Chaos::Storage`.

## Appendix

- [Assertion Reference](./appendix/01-assertion-reference.md): every macro in one table.
- [Crate Map](./appendix/02-crate-map.md): every crate, its features and dependencies.
- [Configuration Reference](./appendix/03-configuration.md): every config struct with defaults.
- [Fault Reference](./appendix/04-fault-reference.md): every fault family with defaults.
- [Glossary](./appendix/05-glossary.md): the terms.
- [Sim Compatibility Checklist](./appendix/06-sim-compatibility.md): ten checks before code runs in a simulation.
- [Calibrating Against a Real Machine](./appendix/07-calibration.md): `moonpool-calibrate` measures a disk and a network.
- [Running Simulation Binaries with xtask](./appendix/08-xtask.md): `cargo xtask sim`.
