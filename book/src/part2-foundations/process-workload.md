# System Under Test vs Test Driver

<!-- toc -->

Every simulation in moonpool has two distinct roles. Understanding this separation is the single most important concept before you write your first line of simulation code.

## Two Perspectives on the Same System

Think about how you test a web server in production. You have the server itself, handling requests, managing connections, storing data. And you have something else: a client, a load generator, a test harness that sends requests and checks responses.

These two things have fundamentally different jobs. The server **is** the system. The test driver **exercises** the system.

Moonpool formalizes this split into two traits: **Process** and **Workload**.

## Process: The Thing You Are Building

A `Process` is your server, your node, your distributed system participant. It is the **system under test**. The code inside a Process is the code you ship to production.

Processes have a difficult life in simulation:

- They **crash**. The simulation kills them without warning.
- They **reboot**. After a crash, the simulation creates a fresh instance from scratch.
- They **lose state**. All in-memory fields vanish on reboot. Only data written to storage survives.
- They **get partitioned**. Network connections break, packets get delayed, peers become unreachable.

This is by design. The whole point of simulation testing is to throw chaos at your server code and see what breaks.

In FoundationDB's simulation, the equivalent is the `fdbd` process. Each simulated FDB node runs the same code as production, but inside a controlled environment where the simulator decides when clocks advance, when packets arrive, and when machines die.

## Workload: The Thing That Tests

A `Workload` is your test driver. It lives outside the system under test. It sends requests, observes responses, and checks that the system behaved correctly.

Workloads have a much easier life:

- They **never crash**. The simulation does not reboot workloads.
- They **survive everything**. Processes come and go, but workloads keep running.
- They **see the whole picture**. Workloads know about all processes, all IPs, the full topology.
- They **judge correctness**. After the simulation ends, workloads run their `check()` method to validate final state.

In FoundationDB's simulation, the equivalent is `tester.actor.cpp`. The tester knows the cluster topology, drives operations against it, and validates results.

## Different Lifecycles

This distinction matters because their lifecycles are completely different.

A **Process** lifecycle looks like this: created from factory, runs, gets killed, factory creates a fresh one, runs again, gets killed again. Each incarnation starts with empty in-memory state. The factory produces a blank slate every time.

A **Workload** lifecycle is linear: `setup()` runs once at the start, `run()` executes the test logic, `check()` validates at the end. One continuous life from start to finish.

Here is the key insight: **a Process does not know when it will die, and a Workload does not care when Processes die**. The Process just tries to do its job correctly. The Workload just keeps sending requests and tracking what happened.

## Why Not One Trait?

You might wonder why we need this separation at all. Why not just have "participants" in the simulation?

Because mixing the two concerns creates a mess. If your server code also has to track test state, you cannot reboot it cleanly. If your test driver also runs server logic, it cannot survive process crashes.

The separation also matches real production architecture. You deploy servers. You run integration tests against them. Different code, different lifecycles, different concerns.

## What Comes Next

The next two chapters cover each trait in detail. We will look at the `Process` trait with its factory pattern and reboot semantics, then the `Workload` trait with its three-phase lifecycle. After that, we will build a complete simulation from scratch.
