# For AI Assistants

<!-- toc -->

This page helps AI assistants navigate the Moonpool book. Each entry links to a chapter with a summary of what it covers. Use this as a routing table to find the right page for a given question.

## Quick Start Routes

- **"What is Moonpool?"** — [The Case for Simulation](./part1-why/01-case-for-simulation.md), then [Why Moonpool Exists](./part1-why/05-why-moonpool.md)
- **"How do I write my first simulation?"** — [Your First Simulation](./part3-building/01-first-simulation.md) and its sub-chapters
- **"How do providers work?"** — [The Provider Pattern](./part2-foundations/04-provider-pattern.md)
- **"How do I add chaos/faults?"** — [Chaos in Moonpool](./part3-building/07-chaos.md)
- **"How do I use assertions?"** — [Assertions: Finding Bugs](./part3-building/12-assertions.md)
- **"How does networking/RPC work?"** — [Simulating the Network](./part4-networking/01-simulating-network.md)
- **"How do virtual actors work?"** — [Virtual Actors](./part5-building-on-top/01-virtual-actors.md)
- **"How do I test an existing app (e.g. axum)?"** — [moonpool-sim Without the Actor System](./part4-integration/01-standalone-sim.md)
- **"How does multiverse exploration work?"** — [Multiverse Exploration](./part5-building-on-top/06-exploration.md)
- **"What assertions are available?"** — [Assertion Reference](./appendix/01-assertion-reference.md)
- **"What configuration options exist?"** — [Configuration Reference](./appendix/03-configuration.md)

## Part I: Why Simulation Testing

- [The Case for Simulation](./part1-why/01-case-for-simulation.md) — Why distributed systems need simulation; the gap between localhost and production; failure statistics
- [Prevention vs Discovery](./part1-why/02-prevention-vs-discovery.md) — Two testing philosophies: regression (prevention) vs generative (discovery)
- [From Mocks to Simulation](./part1-why/03-mocks-to-simulation.md) — Why mocks break at scale; the `#[cfg(test)]` trap; maintenance cost
- [A Brief History](./part1-why/04-history.md) — FoundationDB simulator origins, TigerBeetle storage faults, Orleans actors, Antithesis assertions
- [Why Moonpool Exists](./part1-why/05-why-moonpool.md) — Synthesizing ideas from FDB, TigerBeetle, Orleans, Antithesis into one framework

## Part II: Foundations

- [Determinism as a Foundation](./part2-foundations/01-determinism.md) — Three non-determinism sources: threads, I/O, randomness; why reproducibility matters
- [The Single-Core Constraint](./part2-foundations/02-single-core.md) — Single-threaded execution guarantees one legal ordering; tokio local runtime
- [Seed-Driven Reproducibility](./part2-foundations/03-seeds.md) — One u64 seed controls entire simulation; ChaCha8Rng; cross-platform determinism
- [The Provider Pattern](./part2-foundations/04-provider-pattern.md) — Five traits (Time, Network, Task, Random, Storage) abstract all I/O; swap real vs simulated
- [Quick Start: Swapping Implementations](./part2-foundations/05-providers-quickstart.md) — Practical example: generic function running against TokioProviders or SimProviders
- [Deep Dive: Why Providers Exist](./part2-foundations/06-providers-deepdive.md) — Problems with `#[cfg(test)]` and mocks; providers eliminate both
- [The Five Providers](./part2-foundations/07-provider-traits.md) — TimeProvider, NetworkProvider, TaskProvider, RandomProvider, StorageProvider details
- [System Under Test vs Test Driver](./part2-foundations/08-process-workload.md) — Process (server code) vs Workload (test driver); two distinct roles
- [Process: Your Server](./part2-foundations/09-process.md) — Process trait: `name()`, `run()`; recreated fresh on every boot from factory
- [Workload: Your Test Driver](./part2-foundations/10-workload.md) — Workload trait: `setup()`, `run()`, `check()`; survives reboots; drives and validates

## Part III: Building Simulations

- [Your First Simulation](./part3-building/01-first-simulation.md) — End-to-end walkthrough: KV server process, workload, assertions, builder
- [Defining a Process](./part3-building/02-defining-process.md) — KvServer implementing Process trait; handling TCP; respecting shutdown
- [Writing a Workload](./part3-building/03-writing-workload.md) — KvWorkload tracking state; sending requests; validating responses
- [Configuring the SimulationBuilder](./part3-building/04-simulation-builder.md) — Builder pattern: `.workload()`, `.processes()`, chaos config, iterations
- [Running and Observing](./part3-building/05-running.md) — `cargo xtask sim run`; reading reports; simulation binary structure
- [Chaos Testing vs Simulation](./part3-building/06-chaos-vs-simulation.md) — Chaos engineering (production, reactive) vs simulation (deterministic, proactive)
- [Chaos in Moonpool](./part3-building/07-chaos.md) — Four fault dimensions: buggify, attrition, network faults, storage faults
- [Buggify: Fault Injection](./part3-building/08-buggify.md) — Two-phase activation; testing error paths; FoundationDB-inspired
- [Attrition: Process Reboots](./part3-building/09-attrition.md) — Graceful, crash, wipe reboot types; randomized kills; recovery delay
- [Network Faults](./part3-building/10-network-faults.md) — Connection-level: latency, partition, drops, reordering, clogging
- [Storage Faults](./part3-building/11-storage-faults.md) — TigerBeetle-inspired: corruption, misdirected I/O, phantom writes, sync failures
- [Assertions: Finding Bugs](./part3-building/12-assertions.md) — Record and continue (Antithesis principle); cascade discovery
- [Invariants vs Discovery vs Guidance](./part3-building/13-assertion-concepts.md) — Three assertion categories: invariants, sometimes, numeric
- [Always and Sometimes](./part3-building/14-always-sometimes.md) — `assert_always!` (must hold) vs `assert_sometimes!` (exploration guidance)
- [Numeric Assertions](./part3-building/15-numeric-assertions.md) — `assert_always_less_than!`; watermark tracking; explorer optimizes bounds
- [Compound Assertions](./part3-building/16-compound-assertions.md) — `assert_sometimes_all!` for simultaneous sub-goals; frontier tracking
- [System Invariants](./part3-building/17-invariants.md) — Invariant trait runs after every event; cross-system properties; conservation laws
- [Designing Workloads That Find Bugs](./part3-building/18-designing-workloads.md) — Targeted adversarial design vs white noise; strategy matters
- [Debugging a Failing Seed](./part3-building/19-debugging.md) — Five-step workflow: reproduce, isolate, understand, fix, verify
- [Reproducing with FixedCount](./part3-building/20-reproducing.md) — Pin seed with `set_debug_seeds()` + `set_iterations(1)`; exact replay
- [Reading the Event Trace](./part3-building/21-event-trace.md) — Event queue ordering; `RUST_LOG=trace`; causal chain reconstruction
- [Common Pitfalls](./part3-building/22-pitfalls.md) — Don't `stop().await` in workloads (deadlock); use `drop()` instead

## Part IV: Simulating Existing Applications

- [moonpool-sim Without the Actor System](./part4-integration/01-standalone-sim.md) — Standalone simulation engine for non-actor code (axum, Postgres, etc.)
- [Where to Draw the Line](./part4-integration/02-mock-boundaries.md) — Fakes vs test containers; binary failure limitations
- [Wiring a Web Service](./part4-integration/03-wiring-a-web-service.md) — Worked example: axum service in simulation with Store trait fake, chaos, assertions
- [What You're Testing (and What You're Not)](./part4-integration/04-scope-and-tradeoffs.md) — Tests handler logic and HTTP under chaos; doesn't test TLS, proxies, startup code

## Part V: Networking and RPC

- [Simulating the Network](./part4-networking/01-simulating-network.md) — TCP-level simulation; connection-level faults; FlowTransport architecture
- [Peers and Connections](./part4-networking/02-peers.md) — Logical connection resilience; reconnection on drop; message draining
- [Backoff and Reconnection](./part4-networking/03-backoff.md) — Exponential backoff (FDB pattern); prevents storms; 100ms initial, 30s max
- [Wire Format](./part4-networking/04-wire-format.md) — Packet layout: length, checksum, token, payload; CRC32 validation
- [RPC with #\[service\]](./part4-networking/05-rpc-service.md) — Proc macro: write trait, get client/server/endpoints generated
- [Defining a Service](./part4-networking/06-defining-service.md) — `#[service(id = ...)]` trait, request/response types, serialization
- [Server, Client, and Endpoints](./part4-networking/07-server-client-endpoints.md) — Server setup, client connection, endpoint routing, RequestStream, ReplyPromise
- [Delivery Modes](./part4-networking/08-delivery-modes.md) — Four guarantees: send, try_get_reply, get_reply, get_reply_unless_failed_for
- [Failure Monitor](./part4-networking/09-failure-monitor.md) — Address-level and endpoint-level reachability tracking
- [Designing Simulation-Friendly RPC](./part4-networking/10-designing-rpc.md) — Idempotent design, versioning, bounded retries, deduplication, causality

## Part VI: Building on Top

- [Virtual Actors](./part5-building-on-top/01-virtual-actors.md) — Orleans-inspired identity-based entities; lifecycle management
- [Turn-Based Concurrency](./part5-building-on-top/02-turn-based-concurrency.md) — Per-identity serialization; different identities run in parallel
- [ActorHandler and Lifecycle](./part5-building-on-top/03-actor-handler.md) — `actor_type()`, `dispatch()`, `on_activate()`, `on_deactivate()`, `deactivation_hint()`
- [State Persistence](./part5-building-on-top/04-state-persistence.md) — ActorStateStore trait; ETags; PersistentState wrapper
- [Placement and Directory](./part5-building-on-top/05-placement.md) — ActorDirectory maps ActorId to ActorAddress; lookup, register, unregister
- [Multiverse Exploration](./part5-building-on-top/06-exploration.md) — Checkpoint-and-branch with fork(); timeline tree; exponential trial reduction
- [The Exploration Problem](./part5-building-on-top/07-exploration-problem.md) — Sequential Luck Problem: N unlikely events need exponential trials without branching
- [Fork at Discovery](./part5-building-on-top/08-fork-at-discovery.md) — Unix fork() copies process; reseed with FNV-1a; tree of timelines
- [Coverage and Energy Budgets](./part5-building-on-top/09-coverage-energy.md) — Fixed-count splitting; global energy cap; prevents exponential blowup
- [Adaptive Forking](./part5-building-on-top/10-adaptive-forking.md) — Batch-based exploration; productive marks earn more; barren marks cut early
- [Multi-Seed Exploration](./part5-building-on-top/11-multi-seed.md) — Coverage-preserving seed transitions; selective reset; explored map carries forward

## Appendix

- [Assertion Reference](./appendix/01-assertion-reference.md) — Complete table of 15 assertion macros with behavior and parameters
- [Crate Map](./appendix/02-crate-map.md) — 8-crate workspace diagram and dependency hierarchy
- [Configuration Reference](./appendix/03-configuration.md) — SimulationBuilder methods, ChaosConfiguration, AttritionConfiguration, exploration
- [Fault Reference](./appendix/04-fault-reference.md) — Every fault by category with config fields and defaults
- [Glossary](./appendix/05-glossary.md) — Alphabetical definitions: activation, actor, attrition, buggify, coverage bitmap, etc.
