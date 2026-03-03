# Chaos in Moonpool

<!-- toc -->

## The Philosophy: Make Rare Bugs Common

Real distributed systems fail in ways that are individually rare but collectively inevitable. A network partition happens once a month. A disk corrupts a write once a year. A process crashes during recovery once in a thousand deployments. Individually, each event is unlikely. But multiply enough low-probability events across enough nodes and enough time, and the question is not **if** but **when**.

The core philosophy of chaos in moonpool is simple: **amplify failure probabilities so that rare bugs become common**. If a network partition happens once a month in production, make it happen every few seconds in simulation. If a disk write corrupts once a year, corrupt one every hundred writes. If a process crashes at the worst possible moment once in a thousand runs, make it crash at the worst possible moment every run.

This is not reckless. It is strategic. By making failures frequent, we force the code to handle them **every time**, not just when a developer remembers to write a test for them. And because everything runs inside a deterministic simulation, every failure is reproducible. A bug found at high fault probability is the same bug that would have appeared at low probability in production, just found a thousand times faster.

## Four Dimensions of Chaos

Moonpool provides chaos injection along four distinct dimensions, each targeting a different layer of the system:

### Code Paths: Buggify

Inspired by FoundationDB's BUGGIFY macro, `buggify!()` scatters fault injection points throughout your application code. Each point randomly activates once per simulation run, then fires probabilistically on each call. This forces your code down error paths, timeout branches, and recovery logic that would otherwise require precise failure timing to exercise.

### Infrastructure: Attrition

Processes crash. Processes restart. Sometimes they restart cleanly, sometimes they lose all their data. Attrition automatically cycles your server processes through graceful shutdowns, crashes, and data-wiping restarts during the chaos phase, while respecting a `max_dead` constraint that keeps enough processes alive for the system to remain operational.

### Network Faults

TCP connections fail in subtle ways. Moonpool simulates connection drops, latency spikes, packet corruption, clock drift, partial writes, network partitions, and half-open connections. The simulation operates at the **TCP connection level**, not the packet level, because connection-level faults are what distributed systems actually need to handle.

### Storage Faults

Disks lie. Following TigerBeetle's fault model, moonpool injects read corruption, write corruption, torn writes, misdirected I/O, phantom writes, and sync failures. These are the faults that data-integrity code must survive, and they are nearly impossible to test without simulation.

## Determinism Is Non-Negotiable

Every chaos mechanism in moonpool is controlled by the simulation seed. The same seed produces the same faults in the same order at the same times. This means:

- A failing seed is a permanent, reproducible bug report
- You can debug a failure by replaying it with logging turned up
- Fixing a bug and re-running the seed verifies the fix
- Different seeds explore different fault combinations automatically

This is what separates simulation chaos from production chaos. In production, a network partition happened and something went wrong, but you cannot reproduce the exact sequence of events. In simulation, you hand someone a seed number and they see exactly what you saw.

## The Next Four Chapters

Each dimension of chaos has its own chapter with configuration details, code examples, and the design reasoning behind the choices moonpool makes:

- [Buggify: Fault Injection](buggify.md) covers the `buggify!()` macro and code-level fault injection patterns
- [Attrition: Process Reboots](attrition.md) covers automatic process lifecycle chaos
- [Network Faults](network-faults.md) covers TCP-level network fault simulation
- [Storage Faults](storage-faults.md) covers TigerBeetle-inspired storage fault patterns
