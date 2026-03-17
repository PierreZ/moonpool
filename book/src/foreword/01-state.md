# State of Moonpool

<!-- toc -->

Moonpool is a hobby project under active development. It is not production-ready, and the APIs will change. This book documents the framework as it exists today, with honest markers for what works and what remains experimental.

## What works

The **simulation engine** is the most mature piece. Single-threaded deterministic execution, seed-driven reproducibility, and simulated time advancement all function as described in this book and exercised by the simulation binaries shipped with the repository. A failing seed gives you a reproducible local debugging session, every time.

**Chaos testing** covers network faults (partitions, latency, connection drops, reordering) and storage faults (corruption, torn writes, misdirected I/O). The BUGGIFY-style injection system runs at configurable probability, and the Hurst exponent manipulation produces correlated, cascading failures that mirror real datacenter behavior.

The **assertion suite** implements the full Antithesis-inspired taxonomy: always, sometimes, reachable, unreachable, numeric, and sometimes-all assertions. These live in shared memory and survive fork boundaries for multiverse exploration.

**Transport and RPC** provide a trait-based networking layer with peer connections, wire format, and service definitions via proc-macro. The same code runs against real TCP or the simulated network.

**Fork-based multiverse exploration** is operational: coverage-guided forking, adaptive energy budgets, and multi-seed exploration with coverage preservation across seeds.

## What is experimental

**Per-IP storage scoping** works but the API surface is still evolving. **Parallel exploration** (multi-process exploration across CPU cores) is not yet implemented.

## How to read this book

Part I stands alone as philosophy. You can read it without ever touching moonpool code, and the ideas apply to any simulation framework.

Parts II through V are practical and reflect current APIs. Code examples compile against the latest version, but expect them to evolve. When an API is experimental, the text says so.

Part V on multiverse exploration describes the most novel piece of moonpool. If you are evaluating whether fork-based exploration matters for your use case, start there.
