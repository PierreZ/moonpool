# Chaos Testing vs Simulation

<!-- toc -->

## Two Schools of Breaking Things

There are two common ways to test a distributed system under failure. **Chaos engineering** takes a running system and injects faults into it: kill a VM, cut a network link, fill a disk. **Simulation testing** builds a fake world and runs the system inside it. The faults come from the world itself. Both break things on purpose. They break things in different ways, and the difference decides how we use each one.

## Chaos Engineering: Learning from Production

Chaos engineering, made popular by Netflix's Chaos Monkey, uses production or a production-like staging environment as the laboratory. You pick an experiment ("what happens if we kill this database node?"). You form a hypothesis ("traffic reroutes within 5 seconds"). You run the experiment against real infrastructure and observe.

This approach has real strengths. It tests the **actual system**: real configuration, real dependencies, real network stacks. It finds problems no pre-production test can find: a misconfigured load balancer, a stale DNS cache, a gap in monitoring. When chaos engineering finds something, it matters, because it happened in the real world.

But chaos engineering has structural limits:

- **Non-deterministic.** You cannot reproduce the exact sequence of events that caused a failure. The bug happened once, under conditions you cannot rebuild.
- **Reactive.** You find problems after they exist in production, not before.
- **Slow.** Each experiment takes real time on real machines. One scenario takes minutes. A million scenarios is not practical.
- **Blast radius.** Real users can be affected. An experiment that goes wrong is an outage.

Chaos engineering finds **symptoms**. The database went down and something bad happened. But why? A race? A missing retry? A lock held too long during recovery? Getting from symptom to cause is forensic work after the fact, often with no way to replay the failure.

## Simulation: Breaking Things Before They Exist

Simulation testing takes the opposite path. Instead of injecting faults into a real system, you build a simulated world where faults are a first-class feature. The network drops a connection because the configuration says so. The disk tears a write because the crash model says so. A clock drifts because the simulator makes it drift.

Everything runs in one process on one OS thread, driven by a seeded random number generator. The same seed produces the same execution, every time. A failing test is not a flaky signal. It is a reproducible bug with an exact replay.

The advantages follow directly:

- **Deterministic.** Every failure replays. A failing seed is a permanent regression test.
- **Proactive.** You find bugs before the code reaches production, before staging, often before it leaves your machine.
- **Fast.** No real I/O, no real time. Moonpool simulates hundreds of seconds of cluster time in single-digit real seconds. FoundationDB ran 5 to 10 million simulation runs per night.
- **Wide.** Different seeds explore different fault combinations. Run enough seeds and you cover failure scenarios no one would write a test for by hand.

## Simulation Subsumes Chaos

Here is the key point: **simulation subsumes chaos engineering**. Everything chaos engineering does, simulation does too, with replay, speed and breadth added on top.

Chaos engineering injects a random partition? Simulation injects a random partition, and you can replay the exact moment with the exact timing. Chaos engineering kills a process? Simulation kills it, restarts it, and verifies the recovery path, in milliseconds of real time.

Will Wilson made this concrete with FoundationDB's Sinkhole: a rack of real servers wired to programmable power switches, toggled continuously to validate the real system. Sinkhole never found a database bug that simulation had missed. It found bugs in other software and in hardware. The simulation was a stricter, more adversarial environment than reality.

## Complementary, Not Competing

This does not make chaos engineering useless. The two are complementary:

**Simulation** is for development. It runs on every commit, explores millions of fault scenarios, and catches bugs before they ship. It tests the logic of your system against the worst possible world.

**Chaos engineering** is for production validation. It verifies that your real deployment, with its real configuration and dependencies, behaves as expected. It catches what simulation cannot model: kernel behaviour, cloud provider quirks, misconfigured infrastructure.

Think of simulation as the wind tunnel and chaos engineering as the flight test. You do not skip the wind tunnel because you plan to fly the plane. You do not skip the flight test because the wind tunnel looked good.

## Moonpool's Approach

In moonpool, chaos is a tool **inside** the simulation, not an alternative to it. Process reboots, correlated outages, network faults, storage faults, slow machines and code-path perturbation are all chaos techniques. They run inside a deterministic simulator, so every fault replays and every failure can be debugged.

Faults enter from two sides. The **environment** side is the builder: `enable_chaos` lists the fault surfaces a campaign turns on, and each seed draws its own subset and intensities. The **code** side is the shipped program: `buggify!` takes a rare-but-legal path, and `hint!` names a moment where a fault would be interesting, so the simulator can strike there. Production never sees either. The same binary runs in both worlds, which is the rule [Same Code in Production and Simulation](../part2-foundations/12-same-code.md) explains.

[Chaos in Moonpool](07-chaos.md) maps every surface. The chapters after it take one surface each.
