# Chaos Testing vs Simulation

<!-- toc -->

## Two Schools of Breaking Things

There are two dominant approaches to testing distributed systems under failure. **Chaos engineering** takes a running production system and injects faults into it: kill a VM, drop a network link, fill a disk. **Simulation testing** builds a fake world and runs the system inside it, injecting faults by rewriting the laws of physics. Both break things on purpose. But they break things in fundamentally different ways, and understanding those differences shapes how we use them.

## Chaos Engineering: Learning from Production

Chaos engineering, popularized by Netflix's Chaos Monkey, treats production (or a production-like staging environment) as the laboratory. You pick an experiment ("what happens if we kill this database node?"), form a hypothesis ("traffic reroutes within 5 seconds"), run the experiment against real infrastructure, and observe what happens.

This approach has real strengths. It tests the **actual system** with real configurations, real dependencies, and real network stacks. It catches problems that no amount of pre-production testing would find: misconfigured load balancers, stale DNS caches, monitoring gaps. When chaos engineering finds something, you know it matters because it happened in the real world.

But chaos engineering has structural limitations:

- **Non-deterministic.** You cannot reproduce the exact sequence of events that caused a failure. The bug happened once, under conditions you cannot fully reconstruct.
- **Reactive.** You find problems after they exist in production, not before.
- **Slow.** Each experiment takes real time against real infrastructure. Running one scenario might take minutes. Running a million is not practical.
- **Blast radius.** Real users can be affected. Even with careful scoping, an experiment that goes wrong can cause actual outages.

Chaos engineering finds **symptoms**. The database went down and something bad happened. But why? Was it a race condition? A missing retry? A lock held too long during recovery? Getting from symptom to root cause requires forensic debugging after the fact, often without a way to reproduce the exact failure.

## Simulation: Breaking Things Before They Exist

Simulation testing takes the opposite approach. Instead of injecting faults into a real system, you build a simulated world where faults are a first-class feature. The network drops packets because you told it to. Disks corrupt writes because the configuration says they should. Clocks drift because the simulator makes them drift.

Everything runs in a single process, single-threaded, driven by a seeded pseudorandom number generator. The same seed produces the same execution, every time. A failing test is not a flaky signal. It is a reproducible bug with an exact replay.

The advantages follow directly:

- **Deterministic.** Every failure is reproducible. A failing seed is a permanent regression test.
- **Proactive.** You find bugs before code reaches production, before it reaches staging, often before it leaves a developer's machine.
- **Fast.** No real I/O, no real time. Moonpool simulates hundreds of seconds of cluster behavior in single-digit real seconds. FoundationDB ran 5 to 10 million simulation runs per night.
- **Exhaustive.** Different seeds explore different fault combinations. Run enough seeds and you cover failure scenarios no human would think to write tests for.

## Simulation Subsumes Chaos

Here is the key insight: **simulation subsumes chaos engineering**. Everything chaos engineering does, simulation does too, but with reproducibility, speed, and exhaustiveness added on top.

Chaos engineering injects a random partition? Simulation injects a random partition, and you can replay the exact moment it happened with the exact timing. Chaos engineering kills a process? Simulation kills a process, restarts it, and verifies the recovery path, all in milliseconds of real time.

Will Wilson made this point concrete with FoundationDB's Sinkhole: a rack of real servers wired to programmable power switches, toggled continuously to validate the real system. Sinkhole never found a single database bug that simulation had missed. It only found bugs in other software and in hardware. The simulation was a stricter, more adversarial environment than reality.

## Complementary, Not Competing

This does not mean chaos engineering is useless. The two approaches are complementary:

**Simulation** is for development. It runs on every commit, explores millions of fault scenarios, and catches bugs before they ship. It tests the logic of your system against the worst possible world.

**Chaos engineering** is for production validation. It verifies that your real deployment, with its real configuration and real dependencies, behaves as expected. It catches the things simulation cannot model: actual kernel behavior, real cloud provider quirks, misconfigured infrastructure.

Think of simulation as the wind tunnel and chaos engineering as the flight test. You would never skip the wind tunnel because you plan to fly the plane. And you would never skip the flight test because the wind tunnel looked good.

## Moonpool's Approach

In moonpool, chaos injection is a tool **within** the simulation, not an alternative to it. Network faults, storage corruption, process reboots, code path perturbation: all of these are chaos techniques, but they run inside a deterministic simulator where every fault is reproducible and every failure is debuggable.

The next chapters cover four dimensions of chaos that moonpool provides: `buggify` for code-level fault injection, attrition for process lifecycle chaos, network faults, and storage faults. Each one amplifies failure probabilities so that rare bugs become common, while keeping everything controlled by a single seed.
