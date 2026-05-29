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
| Untracked `tokio::spawn` that bypasses `TaskProvider` | `task.spawn_task(name, fut)` |

`tokio::spawn` directly is acceptable only when we genuinely need a `Send + 'static` driver task and we accept that it does not flow through `TaskProvider` (no naming, no fault injection seam). Default to `spawn_task` so the simulation can see the work.

## 3. Network

| Forbidden | Use instead |
|-----------|-------------|
| `tokio::net::{TcpStream, TcpListener, UdpSocket}` | `network.connect(addr)`, `network.bind(addr)` |
| `std::net::*` for live I/O | `NetworkProvider` |

`NetworkProvider::TcpStream` implements `futures::io::AsyncRead + AsyncWrite`. For libraries that expect tokio I/O traits (hyper, axum), wrap with `tokio_util::compat::Compat` and `hyper_util::rt::TokioIo`. See the axum example for the bridge.

The simulated stream overrides `poll_write_vectored`: each `IoSlice` becomes its own ordered delivery event (so the chaos pack can act on individual segments), and it follows `writev(2)` partial-accept semantics — under send-buffer pressure it accepts the bytes that fit and reports a short count rather than blocking all-or-nothing. `SimTcpStream::is_write_vectored()` returns `true`.

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

- Trait-crossing futures must be **`Send + 'static`**. The sim runtime is `new_current_thread().build()` but spawned futures are Send-bounded.
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
