# Your First Simulation

<!-- toc -->

We have covered the theory. Deterministic execution, providers, the split between Process and Workload. Now we build something real.

## What We Are Building

Over the next four chapters, we will create a complete simulation test for a key-value server. By the end, you will have:

- A **Process** that accepts TCP connections and responds to get/set requests
- A **Workload** that drives random operations and tracks expected state
- **Assertions** that verify correctness during and after the simulation
- A **SimulationBuilder** configuration that runs hundreds of iterations with different seeds

The key-value server is deliberately simple. The interesting part is not the server logic but how the simulation wraps around it, finding bugs you would never catch with unit tests.

## Prerequisites

You need Nix for the development environment. All cargo commands run inside `nix develop`:

```bash
nix develop --command cargo build
```

The simulation binary will live alongside moonpool's existing simulation binaries, managed by xtask:

```bash
cargo xtask sim list    # see what exists
cargo xtask sim run kv  # run our simulation (once we build it)
```

## The Plan

**Chapter: Defining a Process** covers implementing the `Process` trait. We will set up a TCP listener, parse incoming requests, and handle graceful shutdown. The process is a simple in-memory key-value store that loses all state on reboot.

**Chapter: Writing a Workload** covers implementing the `Workload` trait. We will define an operation alphabet (get, set, delete), build a reference model that tracks expected state, and use assertions to catch bugs.

**Chapter: Configuring the SimulationBuilder** covers the builder's fluent API. We will configure the number of processes, set iteration control, add invariants, and enable chaos phases with attrition.

**Chapter: Running and Observing** covers execution and output. We will run the simulation, read the report, understand what success and failure look like, and debug a failing seed.

## Why This Order

We build bottom-up. The Process comes first because it is the system under test, the thing everything else depends on. The Workload comes second because it exercises the Process. The builder ties them together. Running is last because you need all the pieces in place before you can execute.

Each chapter produces code that builds on the previous one. By the end of the fourth chapter, you will have a working simulation you can extend with your own chaos experiments.
