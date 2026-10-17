# The Six Providers

<!-- toc -->

Moonpool puts every interaction between your code and the outside world behind six provider traits: `TimeProvider`, `TaskProvider`, `NetworkProvider`, `RandomProvider`, `StorageProvider` and `SystemProvider`. Each covers one category of I/O. Together they form a complete boundary around your application. A seventh trait, `Resolver`, sits outside the bundle for code that resolves names. The `select!` macro completes the set.

Every provider trait is `Clone + Send + Sync + 'static`. The simulation runs on one thread, but the types are `Send`-bounded so your code stays ordinary: `Arc<RwLock<…>>`, `DashMap` and `Send` task spawning all work. The async methods use native async-fn-in-trait, declared as `-> impl Future<…> + Send` so the bound propagates. There is no `#[async_trait]` and no `?Send` in the provider layer.

## TimeProvider

Every timeout, backoff, heartbeat and lease check goes through `TimeProvider`.

```rust,ignore
pub trait TimeProvider: Clone + Send + Sync + 'static {
    fn sleep(&self, duration: Duration) -> impl Future<Output = Result<(), TimeError>> + Send;
    fn now(&self) -> Duration;
    fn timer(&self) -> Duration { self.now() }
    fn timeout<F, T>(&self, duration: Duration, future: F)
        -> impl Future<Output = Result<T, TimeError>> + Send
    where F: Future<Output = T> + Send, T: Send;
}
```

The `now()` and `timer()` split comes from FoundationDB's `sim2`. `now()` is the exact clock. `timer()` is a default method that returns `now()`, which is what production keeps. The simulation overrides it: `timer()` can run up to `clock_drift_max` (default 100 ms) ahead of `now()`. Use `now()` for scheduling. Use `timer()` for lease expiry and heartbeat deadlines, so clock skew between processes is part of every seed. `sleep` returns `TimeError::Shutdown` when the simulation is gone, and `timeout` returns `TimeError::Elapsed`.

**Production**: `TokioTimeProvider` delegates to `tokio::time::sleep` and `tokio::time::timeout`. `now()` is the elapsed time since the provider was created.

**Simulation**: `SimTimeProvider::sleep` schedules a timer in the `Scheduler<Event>`. When no task is runnable, the simulator pops the next event and jumps the clock to it. Hours of cluster time pass in seconds. `timeout` is a `select! { biased; … }` over the future and a timer, so the event order decides the winner and the race draws nothing.

## TaskProvider

```rust,ignore
pub trait TaskProvider: Clone + Send + Sync + 'static {
    type JoinHandle: Future<Output = Result<(), JoinError>> + Detach + Send + 'static;

    fn spawn_task<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where F: Future<Output = ()> + Send + 'static;

    fn yield_now(&self) -> impl Future<Output = ()> + Send;
}
```

Spawned futures are `Send + 'static`, as `tokio::spawn` requires. The handle resolves with `Err(JoinError::Cancelled)` or `Err(JoinError::Panicked)` when the task did not complete normally. `Detach::detach()` is explicit fire-and-forget: dropping the handle does the same, but `detach()` states the intent and satisfies the `must_use` lint.

**Production**: `TokioTaskProvider` calls `tokio::spawn`. The name feeds two `tracing::trace!` events, at start and at completion, and nothing else.

**Simulation**: `SimTaskProvider` spawns onto the [deterministic executor](./11-executor.md), which stores the name and traces every poll under it. A process's provider carries the **scope** of its boot: when the process is killed, every task spawned through it, and every task those spawned, is dropped. A workload's provider has no scope, because a workload survives everything. A spawned task runs inside the span current at spawn time, so its tracing events keep their process or workload attribution.

## NetworkProvider

```rust,ignore
pub trait NetworkProvider: Clone + Send + Sync + 'static {
    type TcpStream: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    type TcpListener: TcpListenerTrait<TcpStream = Self::TcpStream> + 'static;

    fn bind(&self, addr: &str) -> impl Future<Output = io::Result<Self::TcpListener>> + Send;
    fn connect(&self, addr: &str) -> impl Future<Output = io::Result<Self::TcpStream>> + Send;
}

pub trait TcpListenerTrait: Send + Sync + 'static {
    type TcpStream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    fn accept(&self) -> impl Future<Output = io::Result<(Self::TcpStream, String)>> + Send;
    fn local_addr(&self) -> io::Result<String>;
}
```

The stream types are `futures::io::AsyncRead + AsyncWrite`, the runtime-agnostic traits, so one codec or framing layer works on both backends. `bind`, `connect` and `accept` behave like their tokio counterparts.

**Production**: `TokioNetworkProvider` wraps `tokio::net` through `tokio_util::compat`. Every connected and accepted stream has Nagle's algorithm disabled (`TCP_NODELAY`, best-effort), as FoundationDB's `Net2` does, because request-reply protocols write a small frame and wait for the answer.

**Simulation**: `NetworkSimulation` owns listeners, connections, topology, faults, pending results and wakers. `bind`, `connect` and `accept` park until their scheduled latency expires. Established streams use in-memory buffers with deterministic delivery delays, TCP half-close and fault injection: connection drops, partitions, corruption. Binding port zero on a numeric address assigns a distinct dynamic port, and `local_addr()` returns it. A bare process IP is also a valid address. Dropping a stream with unread bytes resets the peer, as TCP does. `AsyncWrite::close` shuts down the write half only. Each listener has a bounded accept backlog, 128 by default, set per run with `SimulationBuilder::accept_backlog_capacity`. [Network Faults](../part3-building/10-network-faults.md) lists the fault families.

## RandomProvider

```rust,ignore
pub trait RandomProvider: Clone + Send + Sync + 'static {
    fn random<T>(&self) -> T where StandardUniform: Distribution<T>;
    fn random_range<T>(&self, range: Range<T>) -> T where T: SampleUniform + PartialOrd;
    fn random_ratio(&self) -> f64 { self.random() }
    fn random_bool(&self, probability: f64) -> bool { self.random_ratio() < probability }
}
```

Only `random` and `random_range` are required. The trait is fully synchronous: random numbers never need to suspend.

**Production**: `TokioRandomProvider` uses `rand::rng()`, thread-local OS entropy.

**Simulation**: `SimRandomProvider` draws from the one seeded `ChaCha8Rng` stream. Every call is counted, so it is part of the replay and of exploration recipes.

## StorageProvider and StorageFile

```rust,ignore
pub trait StorageProvider: Clone + Send + Sync + 'static {
    type File: StorageFile + 'static;

    fn open(&self, path: &str, options: OpenOptions) -> impl Future<Output = io::Result<Self::File>> + Send;
    fn exists(&self, path: &str) -> impl Future<Output = io::Result<bool>> + Send;
    fn delete(&self, path: &str) -> impl Future<Output = io::Result<()>> + Send;
    fn rename(&self, from: &str, to: &str) -> impl Future<Output = io::Result<()>> + Send;
    fn create_dir_all(&self, path: &str) -> impl Future<Output = io::Result<()>> + Send;
    fn list_dir(&self, path: &str) -> impl Future<Output = io::Result<Vec<String>>> + Send;
    fn sync_dir(&self, path: &str) -> impl Future<Output = io::Result<()>> + Send;
}

pub trait StorageFile: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + Sync + 'static {
    fn sync_all(&self) -> impl Future<Output = io::Result<()>> + Send;
    fn sync_data(&self) -> impl Future<Output = io::Result<()>> + Send;
    fn constraints(&self) -> IoConstraints;
    fn is_direct_io(&self) -> bool;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send;
    fn write_at(&self, offset: u64, buf: &[u8]) -> impl Future<Output = io::Result<usize>> + Send;
    fn write_at_durable(&self, offset: u64, buf: &[u8]) -> impl Future<Output = io::Result<usize>> + Send {
        // default: write_at, then sync_data
    }
    // The zero-copy forms: the buffer moves in and comes back with the count.
    fn read_at_aligned(&self, offset: u64, buf: AlignedBuf) -> impl Future<Output = io::Result<(usize, AlignedBuf)>> + Send {
        // default: read_at on the buffer's bytes
    }
    fn write_at_aligned(&self, offset: u64, buf: AlignedBuf) -> impl Future<Output = io::Result<(usize, AlignedBuf)>> + Send {
        // default: write_at on the buffer's bytes
    }
    fn write_at_durable_aligned(&self, offset: u64, buf: AlignedBuf) -> impl Future<Output = io::Result<(usize, AlignedBuf)>> + Send {
        // default: write_at_durable on the buffer's bytes
    }
    fn size(&self) -> impl Future<Output = io::Result<u64>> + Send;
    fn set_len(&self, size: u64) -> impl Future<Output = io::Result<()>> + Send;
}
```

The split is the rule. The **provider owns the namespace**: open, exists, delete, rename, directory creation, listing and `sync_dir`. The **file owns already-open bytes**. A `delete` or a `rename` over a name removes the name and nothing else. A file opened before it keeps reading and writing the same bytes, as on Unix, until the last name and the last handle are both gone. The namespace is **per process**: two processes that open `data.db` open two files on two disks.

Three distinctions the trait keeps apart, because production keeps them apart:

- A completed write is **visible**, not durable. Only `sync_all` or `sync_data` makes it survive a crash. `write_at_durable` is the one write that is durable on return, and only for its own bytes: on Linux the Tokio backend issues one `pwritev2(RWF_DSYNC)` (a FUA write on a direct-I/O file whose blocks are allocated), which flushes nothing outside its range. The default, and the simulator, run `write_at` then `sync_data`.
- File durability is not **directory-entry** durability. After `open(create)`, `write` and `sync_all`, a crash can still leave no name pointing at the file. `sync_dir` on the parent is the call that makes the name durable, as SQLite and PostgreSQL do after a create, delete or rename.
- `set_len` grows the readable range. It does not allocate, initialize or make the length durable.

`OpenOptions` mirrors `std::fs::OpenOptions` (`read`, `write`, `create`, `create_new`, `truncate`, `append`) and adds `direct_io(DirectIo::{Disabled, Optional, Required})`. Direct I/O is an option on opening an ordinary file, not a second provider: there is no `BlockProvider`. A direct-I/O file reports its alignment through `constraints()` and refuses the stream API with `stream_io_unsupported()`, because a shared cursor cannot honour an alignment. `read_at` and `write_at` are positioned, take `&self`, and may transfer fewer bytes than asked. Each positioned transfer also has an **owned** form over an `AlignedBuf` (`read_at_aligned`, `write_at_aligned`, `write_at_durable_aligned`): the same transfer and contract, with the buffer moved in and handed back beside the count (dropped on error). It exists for zero copy. A backend that runs I/O on another thread cannot move a borrowed slice there, so the borrowed form pays a copy into its own aligned buffer; the owned form moves the caller's buffer instead. A caller that already holds its bytes in an `AlignedBuf` uses it, any other caller keeps the borrowed form, and neither is a different kind of file. `BlockFile<F>` wraps one open file to add block arithmetic and the loops that turn partial transfers into whole ones. It holds no path and opens nothing. `LayoutRegion` lets a format describe where its bytes live, so a fault injector can aim at them. [Storage Faults](../part3-building/11-storage-faults.md) and [Aiming Storage Faults](../part3-building/33-aimed-faults.md) continue there.

**Production**: `TokioStorageProvider` wraps `tokio::fs`. Positioned I/O runs on the blocking pool, and `O_DIRECT` comes from `libc`. The owned transfers move the caller's `AlignedBuf` into the blocking task and back, so `pwrite`, `pwritev2` and `pread` reach the caller's own memory; the borrowed ones copy through a bounce buffer, as a slice cannot cross into a `'static` task.

**Simulation**: the owned transfers keep the trait's defaults, the borrowed transfer on the buffer's bytes, so every fault applies to them unchanged. `StorageEngine` keeps one in-memory image per file. A positioned write is visible to a stream read, and a block write to both. Open handles have independent cursors and pending operations. Every operation returns `Pending` and completes through a scheduled event, with an exact result, so a crash or shutdown completes pending work with errors instead of silence. Each `SimStorageProvider` is scoped to a process IP, which selects that process's disk configuration and degradation episodes. The fault model follows TigerBeetle and FoundationDB: read and write corruption, EIO, the barrier-bounded crash model, misdirected I/O, short transfers, sync lies, unsynced directory-entry loss, and IOPS and bandwidth timing.

## SystemProvider

How busy is this process? FoundationDB reads cumulative counters every few seconds: CPU seconds from `getrusage`, machine CPU from `/proc/stat`, the disk's busy time from `/proc/diskstats`. Two readings and a division give `cpu.usage_cores` and `disk.busy`. `SystemProvider` is that reading:

```rust,ignore
pub trait SystemProvider: Clone + Send + Sync + 'static {
    // Cumulative counters now. `data_dir` selects the disk device.
    fn sample(&self, data_dir: &str) -> io::Result<SystemSample>;
}
```

A `SystemSample` carries the instant `at`, `process_cpu`, the usable `cores`, `machine_busy` and `machine_total`, the run loop's busy time, and the `DiskCounters` of the device that holds `data_dir`: `io_ticks` (time with at least one I/O in flight), `in_flight`, completed `reads` and `writes`, their bytes and their times. The provider does no arithmetic. Every counter only grows, so a counter that goes back tells the caller the window is not valid.

**Production**: `TokioSystemProvider` reads `getrusage(RUSAGE_SELF)`, `/proc/stat` and the `/proc/diskstats` line whose major and minor numbers match `stat(data_dir)`. On btrfs, overlayfs and some containers no line matches, so `disk` is `None`. The run loop's busy time is the busiest tokio worker's. Remember that `io_ticks` is iostat's `%util`: on flash and RAID it reaches 100% long before the device runs out of throughput.

**Simulation**: FoundationDB switches its system monitor off in simulation. Moonpool does the opposite. `StorageEngine` counts, per process, the time its disk has at least one operation in flight, parked operations on a failed disk included, so a stall or throttle episode makes `io_ticks` rise. `SimWorld::disk_counters(ip)` returns the same numbers, so a harness can check a process's report against ground truth. With the [CPU model](../part3-building/26-cpu-and-gray-failure.md) on, every poll of the process's tasks costs simulated time on its one core, and `process_cpu`, `machine_busy` and `run_loop_busy` are that core's busy time. With the model off they stay at zero. The world remembers the last 64 samples per process (`SYSTEM_SAMPLES_KEPT`), readable with `ctx.system_samples(ip)`. A sample draws nothing from the seed.

## The Providers Bundle

```rust,ignore
pub trait Providers: Clone + Send + Sync + 'static {
    type Network: NetworkProvider;
    type Time: TimeProvider;
    type Task: TaskProvider;
    type Random: RandomProvider;
    type Storage: StorageProvider;
    type System: SystemProvider;

    fn network(&self) -> &Self::Network;
    fn time(&self) -> &Self::Time;
    fn task(&self) -> &Self::Task;
    fn random(&self) -> &Self::Random;
    fn storage(&self) -> &Self::Storage;
    fn system(&self) -> &Self::System;
}
```

`TokioProviders::new()` bundles the six production implementations. `SimProviders::new(sim, ip)` bundles the six simulation implementations, scoped to one process IP. Your application code sees `P: Providers` and nothing else.

## Resolver

Name resolution is not in the bundle. Most code never resolves a name, and code that does picks its resolver next to its providers.

```rust,ignore
pub trait Resolver: Clone + Send + Sync + 'static {
    fn resolve(&self, target: &str) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send;
}
```

The target is the same `host:port` string `connect` takes. A numeric target resolves to itself. An `Ok` answer is never empty: a name with no address is a `NotFound` error, so a caller can tell a failed lookup from a failed connection. Caching and time-to-live belong to the caller.

**Production**: `TokioResolver` (feature `tokio-net`) calls `tokio::net::lookup_host`.

**Simulation**: `ScriptedResolver` answers from a shared table that a fault injector or workload edits while the run goes. Answers are immediate and draw no randomness.

## select!

`moonpool_core::select!`, re-exported as `moonpool_sim::select!` and `moonpool::select!`, is tokio's own `select!` macro with one change. tokio's fair mode polls branches starting at a random index drawn from its thread-local RNG, which is seeded from process entropy when there is no tokio runtime. That is a hole in the replay. Moonpool's macro calls tokio's internal expansion with the start offset drawn from `select_support::select_offset`, which moonpool-sim points at the seeded stream for each iteration. Grammar, limits, `if` preconditions and the 64-branch cap are tokio's, byte for byte.

`select! { biased; … }` forwards verbatim to tokio: source-order polling is already deterministic. Use `biased;` when branches have a natural priority, such as a shutdown or timeout guard. Use the default form when branches are peers racing for the same wake, so the seeds explore both winners.

Which macro you get is decided by cargo feature unification. With core's `select` feature alone, the re-export is tokio's macro verbatim. The moment moonpool-sim enters the build graph, even as a dev-dependency, its `deterministic-select` feature replaces it for every crate in that graph. [Same Code in Production and Simulation](./12-same-code.md) explains what that means for a crate's manifest.
