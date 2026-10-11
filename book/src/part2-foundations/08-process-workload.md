# System Under Test vs Test Driver

<!-- toc -->

Every moonpool simulation has two roles. Understanding this split is the one concept to have before you write a line of simulation code.

## Two Perspectives on One System

Think about how you test a web server. You have the server: handling requests, managing connections, storing data. And you have something else: a client, a load generator, a harness that sends requests and checks the answers.

These two have different jobs. The server **is** the system. The driver **exercises** the system.

Moonpool makes the split explicit with two traits: `Process` and `Workload`.

## Process: The Thing You Build

A `Process` is your server, your node, your participant in the distributed system. It is the **system under test**. The code inside a `Process` is the code you ship. There is no simulation-only copy of it.

Processes have a hard life in simulation:

- They **crash**. The simulation kills them without warning, at a timed reboot, at a correlated outage, or at a `hint!` the process named itself.
- They **reboot**. After a kill, the runner calls your factory for a fresh instance.
- They **lose state**. Every in-memory field is gone on reboot. Only what reached the simulated disk survives, and only the part that was synced.
- They **get partitioned, delayed and slowed**. Connections break, packets arrive late, a gray failure makes one machine's CPU, disk or network crawl.

This is the point. The simulation throws chaos at your server and watches what breaks.

In FoundationDB's simulation the equivalent is the `fdbserver` process: the same code as production, inside a world where the simulator decides when clocks advance, when packets arrive and when machines die.

## Workload: The Thing That Tests

A `Workload` is your test driver. It lives outside the system under test. It sends requests, observes answers, and judges whether the system behaved.

Workloads have an easy life:

- They **never crash**. The simulation does not reboot workloads, and a `crash_self` from a workload is an error.
- They **survive everything**. Processes come and go. Workloads keep running.
- They **see the whole picture**. A workload knows every process IP, every group, every tag, and can read the simulator's ground truth: a process's disk counters, its system samples, its current slowness.
- They **judge correctness**. After the run, each workload's `check()` validates the final state, and its verdict counts.

In FoundationDB the equivalent is `tester.actor.cpp`. The tester knows the cluster, drives operations against it, and validates the result.

## Different Lifecycles

The two lifecycles are different in shape.

A **Process** lifecycle is a loop: the factory builds one, it runs, it is killed, the factory builds another, it runs, it is killed. Each incarnation starts with empty memory. Its `ctx.providers()` is fresh each boot, and every task it spawned dies with it.

A **Workload** lifecycle is a line: `setup()` once at the start, `run()` for the test, `check()` at the end. One life from start to finish.

The key point: **a Process does not know when it will die, and a Workload does not care when Processes die.** The Process does its job correctly or it does not. The Workload keeps sending requests and tracking what happened.

## Addresses

The runner gives each role its own IP range, so a log line tells you at a glance who speaks:

| Role | Addresses |
|------|-----------|
| First process group | `10.0.1.1`, `10.0.1.2`, … |
| Second process group | `10.0.2.1`, `10.0.2.2`, … |
| Workloads | `10.0.0.1`, `10.0.0.2`, … |

Each `.processes()` or `.cluster()` call registers one group, named after its `Process::name()`, with its own factory and its own per-seed count. [Process Groups, Tags and Topology](../part3-building/27-process-groups.md) covers multi-role systems.

## Why Not One Trait?

Why not "participants" that do both? Because mixing the two concerns makes a mess. If server code also tracks test state, it cannot be rebooted cleanly. If driver code also runs server logic, it cannot survive a crash.

The split also matches production. You deploy servers. You run integration tests against them. Different code, different lifecycles, different concerns. And it matches the rule in [Same Code in Production and Simulation](./12-same-code.md): the `Process` is the shipped code, and the `Workload` is the only place where test logic lives.

## What Comes Next

The next two chapters cover each trait in detail: `Process` with its factory, its reboots and its context, then `Workload` with its three phases and its operation alphabet. After that, [Part III](../part3-building/01-first-simulation.md) builds a complete simulation from scratch.
