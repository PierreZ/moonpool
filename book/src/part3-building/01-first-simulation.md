# Your First Simulation

<!-- toc -->

Part II covered the theory: deterministic execution, providers, and the split between Process and Workload. Now we build a complete simulation and run it.

## What We Are Building

Over the next five chapters we write a simulation test for a small key-value server. At the end you have:

- A **Process**: the key-value server. It accepts TCP connections and answers get and set requests. The simulator can crash it and reboot it.
- A **Workload**: the test driver. It sends random operations, keeps a reference model of what the server should contain, and checks every answer.
- **Assertions** inside both, so the report tells us what the run found and what it reached.
- A **SimulationBuilder** configuration that runs the pair under chaos for many seeds.

The server is deliberately small. The interesting part is not its logic. It is how the simulation wraps around it and finds bugs that unit tests never reach.

## The Destination

This is the `main` we are working towards. Every call in it gets its own section in the next chapters.

```rust
use std::time::Duration;

use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, SimulationBuilder,
    SimulationError,
};

fn main() -> Result<(), SimulationError> {
    // WARN keeps a 100-seed campaign quiet; raise it when one seed misbehaves.
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = SimulationBuilder::new()
        .processes(3, || Box::new(KvServer))
        .workload(KvWorkload::new(200))
        .enable_chaos([
            Chaos::Network(ChaosMode::Swarm),
            Chaos::Attrition {
                config: Attrition {
                    max_dead: 1,
                    prob_graceful: 0.3,
                    prob_crash: 0.5,
                    prob_wipe: 0.2,
                    recovery_delay_ms: None,
                    grace_period_ms: None,
                    scope: AttritionScope::PerProcess,
                    victims: AttritionVictims::Any,
                },
                mode: ChaosMode::Swarm,
            },
        ])
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(100)
        .run()?;

    report.eprint();
    if report.is_failure() {
        std::process::exit(1);
    }
    Ok(())
}
```

Three servers, one client, network faults and process reboots for 30 simulated seconds, 100 seeds. `run()` returns the report. `is_failure()` is true when a seed failed, an always-assertion was violated, or the assertion table overflowed.

## Prerequisites

The repository builds inside the Nix shell. Every cargo command runs through `nix develop`:

```bash
nix develop --command cargo build
```

Simulation binaries live in `crates/moonpool-sim-examples/src/bin/sim/` and are managed by xtask:

```bash
cargo xtask sim list          # what exists
cargo xtask sim run tonic     # run binaries whose name matches "tonic"
cargo xtask sim run-all       # run every binary
```

Your own simulation can be a plain `#[test]` or a binary. Both call the same builder. [Running and Observing](./05-running.md) covers both routes and the report they print.

## The Plan

1. [Defining a Process](./02-defining-process.md): implement the `Process` trait. Bind a listener, parse requests, honour the shutdown token, lose all memory on reboot.
2. [Writing a Workload](./03-writing-workload.md): implement the `Workload` trait. Pick random operations, keep a reference model, assert on every answer, finish in bounded simulated time.
3. [Configuring the SimulationBuilder](./04-simulation-builder.md): wire the pair together. Every public builder method, with a link to the chapter that explains it.
4. [Running and Observing](./05-running.md): run the campaign, read the report, know when it succeeded, and know what stops it.
5. [Process Groups, Tags and Topology](./27-process-groups.md): grow from one role to many. Groups, tags, failure domains, and how a workload finds each of them.

## Why This Order

We build bottom-up. The Process comes first because it is the system under test. The Workload comes second because it exercises the Process. The builder ties them together. Running comes last because the report only makes sense once every piece is in place.

Each chapter produces code that the next one uses. After [Running and Observing](./05-running.md) you have a working simulation. The chaos chapters that follow, starting with [Chaos in Moonpool](./07-chaos.md), extend it with more fault surfaces.

One rule holds through all of it: the server code is **the code you ship**. It depends on provider traits, never on tokio directly, so the same `KvServer` runs under `TokioProviders` in production and under the simulator's providers here. [Same Code in Production and Simulation](../part2-foundations/12-same-code.md) explains why this is the foundation everything else rests on.
