# The Sim Book

<div style="text-align: center; margin: 2em 0;">
  <img src="images/logo.png" alt="Moonpool logo" style="max-width: 300px;" />
</div>

A guide to building and testing distributed systems with **moonpool**, a deterministic simulation framework for Rust.

Moonpool takes ideas from FoundationDB, TigerBeetle and Antithesis and puts them in one set of Rust crates. You write your system once against provider traits. In production the traits are backed by `TokioProviders`: real TCP, a real disk, a real clock. In tests the same code runs inside a simulator that is deliberately worse than production: partitions, torn writes, slow machines, reboots, all driven by one seed. No `#[cfg(test)]`. No mocks. No `Sim*` copies of your types.

The book has six parts and an appendix:

- **Part I** explains why simulation testing exists and what moonpool offers. You can read it without any code.
- **Part II** covers the foundations: determinism, the provider pattern, and the split between the system under test and the test driver.
- **Part III** builds a simulation step by step: processes, workloads, the `SimulationBuilder`, chaos, assertions, events, metrics and debugging.
- **Part IV** wires existing applications (axum, tonic, hyper) into the simulator and takes the same code to production and to the browser.
- **Part V** covers the raw network layer and the typed RPC crate built on it.
- **Part VI** covers multiverse exploration and the crash-aware journal built on top of the simulator.
- The **Appendix** holds the reference tables: assertions, crates, configuration, faults, the calibration tool and the `xtask` runner.

If you are new, start with [The Case for Simulation](./part1-why/01-case-for-simulation.md). If you already know why you want simulation, jump to [Why Moonpool Exists](./part1-why/05-why-moonpool.md) for the feature tour, then to [Your First Simulation](./part3-building/01-first-simulation.md). AI agents working on a moonpool codebase should read the [Guide for AI Agents](./llms.md).
