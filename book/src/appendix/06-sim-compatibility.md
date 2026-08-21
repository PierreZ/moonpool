# Sim Compatibility Checklist

<!-- toc -->

Consult this list when bringing an existing crate into a moonpool simulation. Determinism is the contract: every source of wall-clock time, real I/O, untracked concurrency, or platform entropy must be routed through a **provider** or replaced with a deterministic alternative. Each section below pairs the calls that break determinism with the moonpool equivalents that preserve it. The [Axum web service example](../../moonpool-sim-examples/src/axum_web.rs) shows the full pattern end-to-end, including how to bridge `futures::io` to tokio-flavored libraries via `tokio_util::compat`.

## 1. Time

| Forbidden | Use instead |
|-----------|-------------|
| `tokio::time::sleep`, `tokio::time::timeout`, `tokio::time::Instant` | `time.sleep(d)`, `time.timeout(d, fut)`, `time.now()` |
| `std::time::Instant::now`, `std::time::SystemTime::now` | `time.now()` for canonical time, `time.timer()` for drifted time |

The `TimeProvider` trait gives us a single seam. In simulation `sleep` advances logical time; in production it falls through to tokio. Never measure elapsed time against a real clock.

## 2. Tasks and concurrency

| Forbidden | Use instead |
|-----------|-------------|
| `std::thread::spawn`, raw OS threads | `task.spawn_task(name, fut)` |
| `tokio::task::spawn_local`, `tokio::task::LocalSet` | `task.spawn_task(name, fut)` (Send-bounded) |
| `tokio::spawn` | `task.spawn_task(name, fut)`, or `moonpool_sim::executor::spawn(name, fut)` in sim-only code |
| Bare `tokio::select!` | `moonpool_sim::select!` / `moonpool::select!` (same grammar) |

There is no tokio runtime inside a simulation: everything runs on the [moonpool deterministic executor](../part2-foundations/11-executor.md), so a direct `tokio::spawn` panics. `moonpool_sim::executor::spawn(name, fut)` is the escape hatch when we genuinely need a raw driver task, but default to `spawn_task` so the same code runs against production providers.

The same goes for branch selection. Bare `tokio::select!` draws its polling offset from OS entropy off a tokio runtime, which breaks seed replay. Moonpool's `select!` keeps the exact tokio grammar but draws the offset from the seed. Write guard-style selects (work vs shutdown or timeout) as `select! { biased; ... }` with the work branch first, and leave peer races (two data sources that can be ready together) in the default form so the seed explores both orders.

## 3. Network

| Forbidden | Use instead |
|-----------|-------------|
| `tokio::net::{TcpStream, TcpListener, UdpSocket}` | `network.connect(addr)`, `network.bind(addr)` |
| `std::net::*` for live I/O | `NetworkProvider` |

`NetworkProvider::TcpStream` implements `futures::io::AsyncRead + AsyncWrite`. For hyper (and therefore axum and tonic), wrap it in `moonpool_hyper::HyperIo`, which presents that shape as hyper's own `rt::Read` and `rt::Write`. That replaces the older two-hop bridge through `tokio_util::compat::Compat` and `hyper_util::rt::TokioIo`, so neither of those crates needs to appear in your dependency list.

For gRPC via tonic, skip `tonic::transport` entirely: it hard-codes `tokio::spawn`, `TokioExecutor`, and `TokioTimer` with no override hooks. Use tonic with `default-features = false` (the runtime-free gRPC framing core) and let `moonpool-hyper` drive hyper for you. `ReconnectingChannel` plays the role `tonic::transport::Channel` plays in production (lazy connect, deterministic backoff, one multiplexed connection shared by every generated client), and `H2Server::serve_connection_with_shutdown` serves each accepted connection with a graceful drain wired to your shutdown token. Underneath, `HyperExecutor` routes hyper's internal h2 tasks to the `TaskProvider` and `HyperTimer` answers hyper's clock reads from the `TimeProvider`, so h2 keepalive ping/pong runs on deterministic sim time. h2 connections are `Send`, so unlike the HTTP/1 case they spawn as ordinary sim tasks. See [The hyper Stack](../part4-integration/08-hyper-stack.md) for the worked example, and `moonpool-sim-examples/src/tonic_grpc.rs` (`cargo xtask sim run tonic-grpc`) for the code: generated protobuf stubs, concurrent unary RPCs multiplexed on one connection, server streaming, deadlines via the time provider, keepalive, reconnection, and Attrition server reboots.

The simulated stream overrides `poll_write_vectored`: each `IoSlice` becomes its own ordered delivery event (so the chaos pack can act on individual segments), and it follows `writev(2)` partial-accept semantics, accepting the bytes that fit under send-buffer pressure and reporting a short count rather than blocking all-or-nothing. Reaching that path takes an IO layer that advertises the capability. `HyperIo` reports no vectored support by default, because futures-io gives it no way to ask the stream, so opt in with `HyperIo::new(stream).with_vectored_writes(true)` (or the `vectored_writes` flag on `ChannelConfig` and `H2ServerConfig`). Both examples switch it on.

## 4. Filesystem and storage

| Forbidden | Use instead |
|-----------|-------------|
| `tokio::fs::*`, `std::fs::*` | `storage.open(path, options)`, `storage.exists`, `storage.delete`, `storage.rename` |
| Direct file handles | `StorageFile` with `sync_all`, `sync_data`, `size`, `set_len` |

Storage operations return `Poll::Pending` and require simulation stepping. See the **Storage Testing Patterns** section of the project `CLAUDE.md` for the step-loop required when driving storage from a test.

## 5. Randomness

| Forbidden | Use instead |
|-----------|-------------|
| `rand::thread_rng`, `rand::random` | `random.random::<T>()`, `random.random_range(r)` |
| `OsRng`, `getrandom`, `/dev/urandom` | `RandomProvider` |
| Any system entropy source | `RandomProvider` |

Every random decision must be seeded by the simulation. A single ungoverned `thread_rng` call is enough to make a seed unreproducible.

## 6. Collections and iteration

| Forbidden | Use instead |
|-----------|-------------|
| `HashMap` / `HashSet` with default `RandomState` | `BTreeMap` / `BTreeSet`, or `HashMap` with a fixed `BuildHasher` |
| Iterating a `HashMap` and acting on order | Sort keys explicitly, or use an ordered map |

`HashMap`'s default hasher randomizes iteration order per process. That is fatal under fork-based exploration where children must replay the parent's behavior.

## 7. Type bounds

- Trait-crossing futures must be **`Send + 'static`**. The sim runs on the single-threaded moonpool executor, but spawned futures are Send-bounded.
- Shared mutable state: **`Arc<RwLock<…>>`**, `Arc<AtomicBool>`, `DashMap`, and similar. Not `Rc<RefCell<…>>`.
- `Process`, `Workload`, `FaultInjector`, and `#[service]` handlers are dyn-stored. Use `#[async_trait]` with `Send + Sync + 'static` supertraits.
- Provider traits (`TimeProvider`, `TaskProvider`, `NetworkProvider`, `RandomProvider`, `StorageProvider`) use native AFIT with `-> impl Future<…> + Send`. No `#[async_trait]`.
- Never hold a `MutexGuard` (or `RwLockGuard`) across `.await`. Drop the guard first, then await.

## 8. External processes and syscalls

| Forbidden | Use instead |
|-----------|-------------|
| `std::process::Command`, `tokio::process::Command` | Encapsulate behind a trait and provide an in-memory fake |
| Raw `mmap`, `socket`, direct `libc` calls | Mediate through a provider or trait you control |
| Linking native libraries that perform their own I/O | Wrap them, or replace with a deterministic fake |

If the dependency cannot be mediated, that call is the boundary of the simulation. Mock it at the highest level you control.

## 9. Observability

| Forbidden | Use instead |
|-----------|-------------|
| `println!`, `eprintln!`, `dbg!` for event-relevant output | `tracing::info!`, `tracing::warn!`, `tracing::error!` |
| Custom log sinks that bypass `tracing` | A `tracing` layer; the sim wires a `SimulationLayer` automatically |

Per the project Rust conventions, every public function carries `#[instrument]`. The sim's tracing layer feeds the **event timeline** invariants read from. Bare `println!` bypasses that capture and hides what happened.

## 10. Assertions and fault injection

- Use `assert_always!`, `assert_sometimes!`, `assert_reachable!`, `assert_unreachable!`, and their numeric and compound variants. Full table in [Assertion Reference](./01-assertion-reference.md).
- Use `buggify!()` and `buggify_with_prob!(p)` for deterministic fault injection at strategic points: error paths, timeouts, retries, resource limits. Decisions are seeded, so failures replay.
- Standard `assert!` / `assert_eq!` still panic and abort the simulation. Prefer moonpool assertions for invariants that should be recorded and explored, not crashed on.
