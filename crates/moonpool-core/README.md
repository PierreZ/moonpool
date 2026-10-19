# moonpool-core

Provider traits and core types for the moonpool simulation framework.

## Same code, two sets of providers

A distributed system touches the outside world through a few operations:
time, tasks, networking, randomness, storage and system counters. This crate
puts each one behind a trait. Application code depends on the traits. In
production it runs on `TokioProviders`. In tests `moonpool-sim` supplies
deterministic providers, and the code does not change. There are no mocks
and no `Sim*` variants of shipped types. This is FoundationDB's rule.

| Trait | Production | Simulation | Purpose |
|-------|------------|------------|---------|
| `TimeProvider` | Wall clock | Logical time, clock drift | `sleep`, `timeout`, `now`, `timer` |
| `TaskProvider` | `tokio::spawn` | Seeded deterministic executor | `spawn_task`, `yield_now`; `spawn_local` via `LocalTaskProvider` |
| `NetworkProvider` | Real TCP | Simulated TCP with faults | `bind`, `connect`, `accept` |
| `RandomProvider` | Thread-local RNG | The run's one seeded stream | `random`, `random_range`, `random_bool` |
| `StorageProvider` | Real filesystem, direct I/O | Simulated disk with faults | `open`, `rename`, `sync_dir`, positioned I/O |
| `SystemProvider` | `getrusage`, `/proc` | Simulated CPU and disk counters | Cumulative CPU and disk busyness |
| `Resolver` (outside the bundle) | OS lookup | Scripted table | `host:port` to addresses |

`Providers` bundles the six provider types into one type parameter.

```rust
use std::time::Duration;

use moonpool_core::TimeError;
use moonpool_core::prelude::*;

async fn backoff<P: Providers>(providers: &P, attempt: u32) -> Result<(), TimeError> {
    let base_ms = 10_u64 << attempt;
    let jitter_ms = providers.random().random_range(0..base_ms);
    providers.time().sleep(Duration::from_millis(base_ms + jitter_ms)).await
}
```

Production calls `backoff(&TokioProviders::new(), 0)`. A simulation calls it
with the simulator's bundle, where the sleep advances logical time.

**Never call tokio directly in application code.** Use
`providers.time().sleep()`, not `tokio::time::sleep()`, and use `select!`
from `moonpool-sim` or `moonpool`, not `tokio::select!`.

## Other core types

- **Storage**: `OpenOptions` with `DirectIo`, `StorageFile` with positioned
  I/O and sync barriers, `IoConstraints` and `AlignedBuf` for direct I/O
  (the `*_aligned` methods move an owned `AlignedBuf` to the kernel and back:
  zero-copy, where a borrowed slice goes through a bounce buffer),
  `BlockFile` for block addressing, and `LayoutRegion` so a storage format
  can tell a fault injector where its bytes live.
- **System**: `SystemSample` and `DiskCounters`, with the `/proc` parsers.
- **Metrics**: `MetricsSource`, the registry-agnostic seam the simulation
  report scrapes, and the `metrics::query` engine (rate, bucketize, fill,
  map, reduce, percentiles).
- **Errors**: `SimulationError`, `TimeError` and `JoinError`.

## Features

| Feature | Default | Adds |
|---------|---------|------|
| `tokio-providers` | yes | `TokioProviders`: all six `tokio-*` features |
| `tokio-task`, `tokio-time`, `tokio-net`, `tokio-fs`, `thread-rng`, `tokio-system` | via umbrella | One production provider each (`tokio-net` also adds `TokioResolver`) |
| `host` | via `tokio-fs` / `tokio-system` | `direct_io` and `host`: the direct-I/O probing and `/proc`, `getrusage`, `statvfs` readers shared by the production backends (`moonpool-compio` enables it alone) |
| `select` | yes | `select!` as tokio's macro, unchanged |
| `deterministic-select` | no | `select!` with a seeded branch offset (enabled by `moonpool-sim`) |

With `default-features = false` the crate holds only the traits and plain
types, and it compiles for `wasm32-unknown-unknown`.

## Documentation

- [The Six Providers](https://pierrez.github.io/moonpool/part2-foundations/07-provider-traits.html) in the Sim Book
- [API documentation](https://docs.rs/moonpool-core)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
