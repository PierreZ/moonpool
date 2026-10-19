# Sim Compatibility Checklist

<!-- toc -->

Use this list when you bring an existing crate into a moonpool simulation. Determinism is the contract: every source of wall-clock time, real I/O, untracked concurrency or platform entropy goes through a **provider** or is replaced by a deterministic alternative. Each table pairs the calls that break replay with the moonpool equivalents. The [axum example](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool-sim-examples/src/axum_web.rs) shows the whole pattern end to end.

## 1. Time

| Forbidden | Use instead |
|-----------|-------------|
| `tokio::time::sleep`, `tokio::time::timeout`, `tokio::time::Instant` | `time.sleep(d)`, `time.timeout(d, fut)`, `time.now()` |
| `std::time::Instant::now`, `std::time::SystemTime::now` | `time.now()` for canonical time, `time.timer()` for the node's drifted clock |

`TimeProvider` is the one seam. In simulation `sleep` advances logical time. In production it falls through to tokio. Never measure elapsed time against a real clock.

## 2. Tasks and Concurrency

| Forbidden | Use instead |
|-----------|-------------|
| `std::thread::spawn`, OS threads | `task.spawn_task(name, fut)` |
| `tokio::task::spawn_local`, `tokio::task::LocalSet` | `ctx.task().spawn_local(name, fut)` for `!Send` futures, else `task.spawn_task(name, fut)` |
| `compio::runtime::spawn` | `task.spawn_task(name, fut)`, or `spawn_local` for `!Send` futures |
| `compio::time::sleep` | `time.sleep(duration)` |
| `tokio::spawn` | `task.spawn_task(name, fut)`, or `moonpool_sim::executor::spawn(name, fut)` in sim-only code |
| `tokio::select!` | `moonpool_sim::select!` or `moonpool::select!` (same grammar) |

There is no tokio runtime inside a simulation. Everything runs on the [deterministic executor](../part2-foundations/11-executor.md), so a bare `tokio::spawn` panics. Bare `tokio::select!` draws its branch offset from OS entropy. Moonpool's `select!` keeps the grammar and draws the offset from the seed. Write guard-style selects (work versus shutdown) as `select! { biased; .. }` with the work branch first. Leave peer races in the default form so the seed explores both orders.

## 3. Network

| Forbidden | Use instead |
|-----------|-------------|
| `tokio::net::{TcpStream, TcpListener, UdpSocket}` | `network.connect(addr)`, `network.bind(addr)` |
| `std::net::*` for live I/O | `NetworkProvider` |
| `tokio::net::lookup_host`, `ToSocketAddrs` | a `Resolver`: `TokioResolver` in production, `moonpool_sim::ScriptedResolver` in simulation |
| `hyper_util::rt::TokioIo`, `tokio_util::compat` | `moonpool_hyper::HyperIo` |
| `tonic::transport` | tonic with `default-features = false`, `ReconnectingChannel`, `H2Server` |

`NetworkProvider::TcpStream` implements `futures::io::AsyncRead + AsyncWrite`. `HyperIo` presents it as hyper's `rt::Read` and `rt::Write`. Opt into vectored writes with `HyperIo::new(stream).with_vectored_writes(true)` or the `vectored_writes` flag on `ChannelConfig` and `H2ServerConfig`; the simulated stream then delivers each `IoSlice` as its own event. `HyperExecutor` routes hyper's h2 tasks to the `TaskProvider`, `HyperTimer` answers its clock reads from the `TimeProvider`. See [The hyper Stack](../part4-integration/08-hyper-stack.md).

## 4. Filesystem and Storage

| Forbidden | Use instead |
|-----------|-------------|
| `tokio::fs::*`, `std::fs::*` | `storage.open(path, OpenOptions)`, `exists`, `delete`, `rename`, `create_dir_all`, `list_dir`, `sync_dir` |
| Direct file handles, `O_DIRECT` by hand | `StorageFile` (`read_at`, `write_at`, `write_at_durable`, their zero-copy `*_aligned` forms over `AlignedBuf`, `sync_all`, `sync_data`, `size`, `set_len`), `OpenOptions::direct_io(DirectIo::..)`, `BlockFile<F>` |

Storage operations return `Poll::Pending` and complete through scheduled events. A low-level test must step `SimWorld` while it polls (the `drive` helper in the root `AGENTS.md`). File durability is not directory-entry durability: `sync_dir` after a create or rename.

## 5. System Metrics

| Forbidden | Use instead |
|-----------|-------------|
| `getrusage`, `/proc/stat`, `/proc/diskstats`, `sysconf` | `system.sample(data_dir) -> SystemSample` |

`SystemProvider::sample` returns process CPU, core count, machine busy and total time, run-loop busy time, optional `DiskCounters` and optional `DiskSpace` (`statvfs`). In simulation it reports the CPU model's charges and the simulated disk, so an admission controller that watches its own load sees the chaos.

## 6. Randomness

| Forbidden | Use instead |
|-----------|-------------|
| `rand::thread_rng`, `rand::random`, random UUIDs | `random.random::<T>()`, `random.random_range(r)`, `random.random_bool(p)`, `random.random_ratio()` |
| `OsRng`, `getrandom`, `/dev/urandom` | `RandomProvider` |

One ungoverned `thread_rng` call makes a seed unreproducible.

## 7. Collections and Iteration

| Forbidden | Use instead |
|-----------|-------------|
| `HashMap`, `HashSet` | `BTreeMap`, `BTreeSet` |
| Iterating an unordered collection from a dependency | Copy into a `Vec` and sort before acting |

`HashMap` randomizes iteration order per process. That is fatal under replay and forked exploration. The workspace `clippy.toml` disallows both types.

## 8. Fault Sites and Probes

| In library code | Purpose |
|-----------------|---------|
| `hint!("entries written, not synced")` | Name a crash moment. A no-op in production. The simulator may reboot the process there. |
| `buggify!()`, `buggify_with_prob!(p)`, `buggify_fault_with_prob!(p)`, `buggify_named!("label", p)` | Fire a rare path on seeded coins. Inert in production. |
| `cpu_charge(Duration)` | Account CPU work the poll model cannot see (a checksum, a compaction). |
| `reachable!("msg")`, `sometimes!(cond, "msg")` | Coverage probes from `moonpool-assertions`, without a `moonpool-sim` dependency. |

All of these come from `moonpool-buggify` and `moonpool-assertions`, which have zero dependencies, so a library can carry them without pulling in the simulator.

## 9. Type Bounds

- Futures that cross a trait are `Send + 'static`. The executor is single-threaded, but spawned futures are Send-bounded.
- Shared state: `Arc<RwLock<..>>`, `Arc<AtomicBool>`, `DashMap`. Not `Rc<RefCell<..>>`.
- `Process`, `Workload` and `FaultInjector` are dyn-stored: `#[async_trait]` with `Send + Sync + 'static`.
- Provider traits use native `async fn` with `-> impl Future<..> + Send`. No `#[async_trait]`.
- Never hold a `MutexGuard` or `RwLockGuard` across `.await`.
- No `unwrap()`. For lock poison use `.expect("RwLock poisoned: prior task panicked")`.

## 10. External Processes and Syscalls

| Forbidden | Use instead |
|-----------|-------------|
| `std::process::Command`, `tokio::process::Command` | A trait with an in-memory fake |
| Raw `mmap`, `socket`, direct `libc` calls | A provider or a trait you control |
| Native libraries that do their own I/O | Wrap them, or replace them with a deterministic fake |

When a dependency cannot be mediated, that call is the boundary of the simulation. Mock it at the highest level you control. See [Where to Draw the Line](../part4-integration/02-mock-boundaries.md).

## 11. Observability

| Forbidden | Use instead |
|-----------|-------------|
| `println!`, `eprintln!`, `dbg!` for facts an invariant needs | `tracing::info!(target: "raft", term, leader = %ip, "leader_elected")` |
| A custom log sink that bypasses `tracing` | A `tracing` layer; the runner installs `SimulationLayer` |

The layer captures INFO and above with a constant message, inside a process or workload task. Use `%` for strings. Do not add a `time_ms` field; the layer stamps simulated time.

## 12. Assertions

- Use `assert_always!`, `assert_sometimes!`, `assert_reachable!`, `assert_unreachable!` and the numeric and compound variants. Full table in the [Assertion Reference](./01-assertion-reference.md).
- Standard `assert!` and `assert_eq!` panic and abort the run. Prefer moonpool assertions for invariants that should be recorded and explored.
- Never delete an assertion that catches a bug. Fix the bug.
