# Why Moonpool Exists

<!-- toc -->

After ten years of operating distributed systems, one pattern keeps repeating: the bugs that hurt most are the ones nobody imagined.

You build a system. You test the scenarios you can think of. You deploy. Then a network partition overlaps with a retry storm during a rolling upgrade, and the system enters a state no developer on the team anticipated. The code was not wrong. It just was not tested against that particular combination of events.

The combinatorial problem makes this inevitable. A simple e-commerce API with six variable dimensions (user types, payment methods, delivery options, promotions, inventory status, currencies) requires 648 unique test combinations for basic coverage. Adding one option to each dimension pushes it past 4,000. A distributed system with network partitions, node crashes, disk faults, and clock drift has orders of magnitude more dimensions. You cannot test what you do not know. Manual tests ensure regression coverage, not absence of bugs.

That realization is what led to moonpool: the testing toolbox I wish I had when I started building distributed systems.

## What I found

The answer already existed, scattered across several projects and decades of work.

**FoundationDB** had shown that simulating a distributed system's network inside a single-threaded process, with deterministic fault injection and seed-driven reproducibility, could find more bugs in a night than production found in a year. Their BUGGIFY technique and Hurst exponent manipulation made the simulated world worse than reality. Their Sinkhole cluster never found a database bug that simulation missed.

**TigerBeetle** extended the simulation philosophy to storage. Where FoundationDB focused on network faults (partitions, latency, connection drops), TigerBeetle modeled disk-level faults drawn from real hardware failure modes: torn writes, misdirected reads, phantom writes, read corruption, sync failures, uninitialized memory. A financial transactions database cannot afford to trust that disks behave according to spec.

**Antithesis** generalized FoundationDB's assertion system into a declarative SDK. Instead of hand-coding what to explore, you declare properties: `Sometimes` (this condition should fire), `Always` (this invariant must hold), `SometimesAll` (drive exploration along a frontier). The platform figures out how to reach those states. Their work on NES games demonstrated that coverage-guided forking and adaptive exploration could beat complex state spaces with minimal domain knowledge.

## What moonpool synthesizes

Moonpool brings these ideas together in a single Rust framework.

From FoundationDB: the **simulation engine**. Single-threaded deterministic execution. A seeded PRNG controlling all network timing, fault injection, and process scheduling. Simulated time that jumps forward when all tasks are blocked, compressing hours of cluster behavior into seconds. The same code runs against real networking or the simulated network, swapped at the provider level.

From TigerBeetle: **storage fault injection**. Simulated disk operations that can corrupt reads, tear writes, fail syncs, and misdirect I/O. Not just network chaos but disk chaos, because real systems fail at both layers.

From Antithesis: the **assertion suite** and **fork-based exploration**. Always, sometimes, reachable, unreachable, numeric, and frontier assertions that live in shared memory. Coverage-guided forking that branches the simulation at interesting points, exploring multiple futures from a single state. Adaptive energy budgets that allocate exploration effort where coverage is still improving.

## Where moonpool sits

Moonpool is a **library-level** simulation framework. There is no hypervisor, no custom virtual machine, no special runtime. You add a dependency to your Rust project, implement a few provider traits, and your system becomes simulatable.

This is a deliberate tradeoff. Antithesis's hypervisor can simulate **any** software without modification. Moonpool requires that your code use provider traits for I/O, which means designing for simulation from the start or refactoring existing code. In exchange, you get zero-cost abstractions in production, full control over fault injection, and the ability to run simulations as ordinary `cargo test` invocations. No infrastructure to deploy. No service to call. Everything runs in your CI pipeline.

The provider pattern means your production binary and your simulation binary share the same application logic. No `#[cfg(test)]`. No conditional compilation. The same code, tested in a world that is deliberately worse than production.

## What makes it different

Many simulation frameworks stop at network-level fault injection. Moonpool adds two things.

First, **storage simulation** at the same fidelity as network simulation. Disk faults drawn from the TigerBeetle and FoundationDB fault models, deterministically controlled by the same seed that controls the network.

Second, **fork-based multiverse exploration**. When the simulation reaches an interesting state (a new assertion fires, a coverage bit flips, a numeric watermark improves), moonpool forks the process. The parent continues its original trajectory. The child explores from the interesting state with a different seed. This turns a single simulation run into a tree of timelines, each branching at points of maximum discovery potential.

This is not a gimmick. The **sequential luck problem** (finding a bug that requires getting lucky multiple times in sequence, where the probability is p^n) is the central bottleneck in simulation-based testing. Fork-based exploration attacks it directly: instead of hoping a single timeline stumbles through all the right doors, we branch at each door and explore both sides.

Part VI covers multiverse exploration in depth. For now, the key point is that moonpool is not just a simulation engine. It is a simulation engine with a built-in search strategy for the state spaces that matter most: the ones where bugs require sequences of unlikely events to surface.
