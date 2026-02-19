# Moonpool

Deterministic simulation testing for distributed systems in Rust. Write your distributed system once, test it with deterministic simulation and chaos injection, then deploy with real networking — using identical application code.

Inspired by [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html) and [Antithesis](https://antithesis.com/).

> **Note:** This is a hobby-grade project under active development.

## Architecture

```text
moonpool                          Facade crate + virtual actors
├── moonpool-transport            RPC, peer connections, wire format
│   └── moonpool-transport-derive #[service] and #[actor_impl] proc-macros
├── moonpool-sim                  Simulation engine, chaos testing, assertions
│   └── moonpool-explorer         Fork-based multiverse exploration
└── moonpool-core                 Provider traits and core types
```

## Which Crate to Use

| Use case | Crate |
|----------|-------|
| Full framework (recommended) | `moonpool` |
| Provider traits only | `moonpool-core` |
| Simulation without transport | `moonpool-sim` |
| Transport without simulation | `moonpool-transport` |
| Fork-based exploration internals | `moonpool-explorer` |
| Proc-macro internals | `moonpool-transport-derive` |

## Key Features

- **Provider pattern** — Application code depends on traits (`TimeProvider`, `NetworkProvider`, `TaskProvider`, `RandomProvider`, `StorageProvider`), not concrete implementations. Same code runs in simulation and production.
- **Deterministic simulation** — Same seed = identical execution. Logical time skips idle periods. Years of uptime simulated in seconds.
- **Chaos testing** — Network delays, disconnects, partitions, bit flips, partial writes, storage corruption. `buggify!` fires with 25% probability at fault injection points.
- **Assertion suite** — 15 Antithesis-style assertion macros (`assert_always!`, `assert_sometimes!`, numeric comparisons, compound assertions). Multi-seed testing runs until all `sometimes` assertions fire.
- **Virtual actors** — Orleans-style actors with turn-based concurrency, lifecycle hooks (`on_activate`/`on_deactivate`), persistent state with optimistic concurrency, and per-identity concurrent processing.
- **Fork-based exploration** — When assertions discover new behavior, `fork()` explores alternate timelines with different RNG seeds. Adaptive energy budgets and coverage bitmaps guide exploration.
- **`#[service]` macro** — Auto-generates RPC server/client boilerplate (`&self` methods) or virtual actor refs and dispatch (`&mut self` methods) from a single trait definition.

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

Each crate has detailed rustdoc with architecture diagrams and usage examples:

```bash
cargo doc --open
```

- [API Documentation](https://docs.rs/moonpool)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
