# Moonpool

Deterministic simulation testing for distributed systems in Rust. Write your distributed system once, test it with deterministic simulation and chaos injection, then deploy with real networking — using identical application code.

Inspired by [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html) and [Antithesis](https://antithesis.com/).

> **Note:** This is a hobby-grade project under active development.

## Architecture

```text
moonpool                          Facade crate (features: sim / tokio / transport)
├── moonpool-transport            RPC, peer connections, wire format
│   └── moonpool-transport-derive #[service] proc-macro
├── moonpool-sim                  Simulation engine, chaos testing, assertion wiring
│   ├── moonpool-assertions       Assertion accounting (pure std, zero deps, wasm-able)
│   └── moonpool-explorer         Fork-based multiverse exploration (optional, libc)
└── moonpool-core                 Provider traits and core types
```

The simulation runtime compiles to `wasm32-unknown-unknown` (build `moonpool-sim`
with `--no-default-features`); only the fork-based explorer is Linux-first.

## Which Crate to Use

| Use case | Crate |
|----------|-------|
| Full framework (recommended) | `moonpool` |
| Provider traits only | `moonpool-core` |
| Simulation without transport | `moonpool-sim` |
| Transport without simulation | `moonpool-transport` |
| Assertion accounting only | `moonpool-assertions` |
| Fork-based exploration internals | `moonpool-explorer` |
| Proc-macro internals | `moonpool-transport-derive` |

## Using in Production

The code you test is the code you ship — write it once against the provider
traits, then deploy on the real `TokioProviders` backend. Keep the simulation
runtime and the fork-based explorer out of your release binary with a lean
dependency stanza:

```toml
[dependencies]
moonpool = { version = "0.8", default-features = false, features = ["tokio", "transport"] }
```

That pulls the provider contract, `TokioProviders`/`TokioTransport`, and the
transport layer — no `moonpool-sim`, no `moonpool-explorer`, no `libc` fork
machinery. See [`moonpool/examples/retrying_worker.rs`](moonpool/examples/retrying_worker.rs)
for a worker that runs on Tokio in `main` and is driven through the simulator by
its own `#[test]`, and the "Using Providers in Production" chapter of the book.

## Key Features

- **Provider pattern** — Application code depends on traits (`TimeProvider`, `NetworkProvider`, `TaskProvider`, `RandomProvider`, `StorageProvider`), not concrete implementations. Same code runs in simulation and production.
- **Deterministic simulation** — Same seed = identical execution. Logical time skips idle periods. Years of uptime simulated in seconds.
- **Chaos testing** — Network delays, disconnects, partitions, bit flips, partial writes, storage corruption. `buggify!` fires with 25% probability at fault injection points.
- **Assertion suite** — 15 Antithesis-style assertion macros (`assert_always!`, `assert_sometimes!`, numeric comparisons, compound assertions). Multi-seed testing runs until all `sometimes` assertions fire.
- **Fork-based exploration** — When assertions discover new behavior, `fork()` explores alternate timelines with different RNG seeds. Adaptive energy budgets and coverage bitmaps guide exploration.
- **`#[service]` macro** — Auto-generates RPC server/client boilerplate from a single trait definition.

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
