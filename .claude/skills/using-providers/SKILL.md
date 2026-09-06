---
name: using-providers
description: Write code against moonpool's provider traits instead of tokio - TimeProvider, TaskProvider, NetworkProvider, RandomProvider, StorageProvider, the Providers bundle, moonpool::select!, and the TokioProviders vs SimProviders split. Use whenever code needs sleep, timeout, spawn, TCP, randomness, files or select! in anything that runs under simulation, when migrating existing tokio code, or when a compile error mentions Send bounds on a provider future.
---

# Using providers

The executor is single-threaded and deterministic; every tokio primitive
bypasses it and either panics (`tokio::spawn` with no runtime) or injects real
time and real randomness that no seed can replay. Providers are the only door.
Production passes `TokioProviders`, simulation passes `SimProviders`, and the
code between them is written once, generic over `P: Providers`.

## The mapping

| Instead of | Write | Trait (`crates/moonpool-core/src/`) |
|---|---|---|
| `tokio::time::sleep(d)` | `time.sleep(d).await?` | `TimeProvider` (`time.rs`) |
| `tokio::time::timeout(d, f)` | `time.timeout(d, f).await?` | `TimeProvider` |
| `Instant::now()` | `time.now()` for scheduling, `time.timer()` for app-visible clocks (skewable) | `TimeProvider` |
| `tokio::spawn(f)` | `task.spawn_task("name", f)`; `.detach()` to fire and forget | `TaskProvider` (`task.rs`) |
| `tokio::task::yield_now()` | `task.yield_now().await` | `TaskProvider` |
| `TcpListener::bind` / `TcpStream::connect` | `network.bind(addr).await?` / `network.connect(addr).await?` | `NetworkProvider` (`network.rs`) |
| `rand::thread_rng()` | `random.random::<T>()`, `random_range(r)`, `random_ratio()`, `random_bool(p)` | `RandomProvider` (`random.rs`) |
| `tokio::fs::File` | `storage.open(path, OpenOptions::..).await?`, `exists`, `delete`, `rename` | `StorageProvider` (`storage.rs`) |
| `tokio::select!` | `moonpool_sim::select!` (or `moonpool::select!`) | `select.rs` |

Get them from a bundle: `providers.time()`, `.task()`, `.network()`,
`.random()`, `.storage()`; inside a `Process`/`Workload`, `ctx.time()` and
friends are the same accessors on `SimContext`.

## Shapes that surprise people

- The traits are native `async fn` in trait (AFIT), `Clone + Send + Sync +
  'static`, returning `impl Future<..> + Send`. Write your own generic code the
  same way: `fn f(&self) -> impl Future<Output = T> + Send` in the trait, plain
  `async fn` in the impl.
- Network streams implement **`futures::io`** (`AsyncRead`/`AsyncWrite`), not
  `tokio::io`. hyper integration goes through `moonpool_hyper::HyperIo`.
- `select!` is tokio's expansion with a seeded start offset: same syntax, but
  the branch order comes from the shared stream, so replay is exact. A
  `tokio::select!` in simulated code is a determinism hole.
- Storage operations complete through scheduled events, so a test that drives
  a provider future by hand must step the world (`sim.step()`) while polling;
  the root `AGENTS.md` shows the `drive` helper.
- `time.timer()` drifts under clock-skew faults while `time.now()` does not;
  use `timer()` for anything the application would read from a wall clock.

## Features

`moonpool-core` is wasm-clean with all features off. `tokio-providers` is the
production umbrella (granular `tokio-task`/`-time`/`-net`/`-fs`/`-random`
exist for consumers that need one slot); `select` re-exports tokio's macro
verbatim for production, `deterministic-select` is what moonpool-sim enables.
Production graphs use `moonpool = { default-features = false, features = ["tokio"] }`.

Book: `book/src/part2-foundations/04-provider-pattern.md` through
`07-provider-traits.md`, `part4-integration/05-production.md`,
`06-migrating-existing-code.md`, `appendix/06-sim-compatibility.md`.
