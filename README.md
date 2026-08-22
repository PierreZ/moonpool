# Moonpool

Deterministic simulation testing for distributed systems in Rust. Write your
system once against provider traits, test it with deterministic simulation and
chaos injection, then deploy the same logic with real networking and storage.

Inspired by [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html) and [Antithesis](https://antithesis.com/).

> **Note:** This is a hobby-grade project under active development.

## Architecture

```text
moonpool                          Facade crate (features: sim / tokio / hyper)
├── moonpool-sim                  Simulation engine, chaos testing, assertion wiring
│   ├── moonpool-assertions       Assertion accounting (pure std, zero deps, wasm-able)
│   └── moonpool-explorer         Frontier-based exploration controller (optional, libc)
├── moonpool-hyper                hyper 1.x: HTTP/gRPC over provider streams (opt-in)
└── moonpool-core                 Provider traits and core types
```

The simulation runtime compiles to `wasm32-unknown-unknown` (build `moonpool-sim`
with `--no-default-features`); the explorer runs on Linux and macOS.

## Which Crate to Use

| Use case | Crate |
|----------|-------|
| Full framework (recommended) | `moonpool` |
| Provider traits only | `moonpool-core` |
| Deterministic simulation | `moonpool-sim` |
| An HTTP/2 stack (tonic, axum, hyper) on the providers | `moonpool` with feature `hyper`, or `moonpool-hyper` |
| Assertion accounting only | `moonpool-assertions` |
| Exploration controller internals | `moonpool-explorer` |

## Using in Production

The code you test is the code you ship — write it once against the provider
traits, then deploy on the real `TokioProviders` backend. Keep the simulation
runtime and the explorer out of your release binary with a lean
dependency stanza:

```toml
[dependencies]
moonpool = { version = "0.8", default-features = false, features = ["tokio"] }
```

That pulls the provider contract and `TokioProviders`, including real TCP and
filesystem implementations, without `moonpool-sim`, `moonpool-explorer`, or
`libc` fork machinery. Add `hyper` when the application uses the HTTP/gRPC
integration. See [`crates/moonpool/examples/retrying_worker.rs`](crates/moonpool/examples/retrying_worker.rs)
for a worker that runs on Tokio in `main` and is driven through the simulator by
its own `#[test]`, and the "Using Providers in Production" chapter of the book.

## Key Features

- **Provider pattern** — Application code depends on traits (`TimeProvider`, `NetworkProvider`, `TaskProvider`, `RandomProvider`, `StorageProvider`), not concrete implementations. Same code runs in simulation and production.
- **Deterministic simulation** — Same seed = identical execution. Logical time skips idle periods. Years of uptime simulated in seconds.
- **Chaos testing** — Network delays, disconnects, partitions, bit flips, partial writes, storage corruption. `buggify!` fires with 25% probability at fault injection points.
- **Assertion suite** — 15 Antithesis-style assertion macros (`assert_always!`, `assert_sometimes!`, numeric comparisons, compound assertions). Multi-seed testing runs until all `sometimes` assertions fire.
- **Frontier exploration** — When assertions discover new behavior, the explorer remembers the replayable recipe and schedules bounded continuations from it. A fixed pool of forked workers executes the timelines; the logical exploration tree can be huge while live processes stay at `1 + workers`.
- **Raw and ecosystem networking** — Exercise your own TCP protocol directly, or run real hyper, axum, and tonic stacks over simulated streams.

## Quick Start

```bash
# Enter development environment (Nix required)
nix develop

# Run tests
nix develop --command cargo nextest run

# Build documentation
nix develop --command cargo doc --open
```

## Documentation

- [**The Sim Book**](https://pierrez.github.io/moonpool/) — User guide covering philosophy, architecture, and practical details
- [API Documentation](https://docs.rs/moonpool) — Rustdoc with architecture diagrams and usage examples
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
