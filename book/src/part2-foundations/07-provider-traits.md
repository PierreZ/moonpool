# The Five Providers

<!-- toc -->

Moonpool abstracts every interaction between your code and the outside world into five provider traits. Each trait covers one category of I/O. Together, they form a complete boundary around your application, giving the simulator full control over every source of non-determinism.

The sim runs single-threaded on the [moonpool deterministic executor](./11-executor.md), but every provider trait is **`Send + Sync + 'static`**. One OS thread runs everything for determinism, yet the **types** are Send-bounded so customer code stays normal: `Arc<RwLock<…>>`, `DashMap`, `Arc<AtomicBool>`, and Send-bounded task spawning all just work. The async methods use **native AFIT** (`async fn` in trait) with explicit `-> impl Future<…> + Send` desugarings to propagate the Send bound, so no `#[async_trait]` and no `?Send` anywhere in the provider layer.

## TimeProvider

Time is the most pervasive dependency in distributed systems. Every timeout, backoff, heartbeat, and lease check goes through `TimeProvider`.

```rust
pub trait TimeProvider: Clone + Send + Sync + 'static {
    /// Sleep for the specified duration.
    fn sleep(
        &self,
        duration: Duration,
    ) -> impl Future<Output = Result<(), TimeError>> + Send;

    /// Get exact current time.
    fn now(&self) -> Duration;

    /// Get drifted timer time (simulates clock drift between nodes).
    /// Defaults to `now()`; the simulation overrides it.
    fn timer(&self) -> Duration {
        self.now()
    }

    /// Run a future with a timeout.
    fn timeout<F, T>(
        &self,
        duration: Duration,
        future: F,
    ) -> impl Future<Output = Result<T, TimeError>> + Send
    where
        F: Future<Output = T> + Send,
        T: Send;
}
```

The distinction between `now()` and `timer()` is borrowed from FoundationDB's `sim2`. `timer()` is a default method that returns `now()`, which is what production keeps; only the simulation overrides it. In simulation, `timer()` can drift up to 100ms ahead of `now()`, testing how your code handles clock skew between processes. Use `now()` for event scheduling. Use `timer()` for application-level time checks like lease expiry and heartbeat deadlines.

**Production**: `TokioTimeProvider` delegates `sleep` to `tokio::time::sleep`, `timeout` to `tokio::time::timeout`, and `now` to `std::time::Instant::elapsed`.

**Simulation**: Sleep schedules a `Timer` in the global `Scheduler<Event>`.
The scheduler owns monotonic logical time and same-time FIFO sequence order.
When all tasks are blocked, the simulator performs "time travel" by popping the
next scheduled event. This compresses hours of simulated cluster time into
seconds of wall-clock time.

## NetworkProvider

```rust
pub trait NetworkProvider: Clone + Send + Sync + 'static {
    type TcpStream: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    type TcpListener: TcpListenerTrait<TcpStream = Self::TcpStream> + 'static;

    /// Create a TCP listener bound to the given address.
    fn bind(
        &self,
        addr: &str,
    ) -> impl Future<Output = io::Result<Self::TcpListener>> + Send;

    /// Connect to a remote address.
    fn connect(
        &self,
        addr: &str,
    ) -> impl Future<Output = io::Result<Self::TcpStream>> + Send;
}

pub trait TcpListenerTrait: Send + Sync + 'static {
    type TcpStream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// Accept a single incoming connection.
    fn accept(
        &self,
    ) -> impl Future<Output = io::Result<(Self::TcpStream, String)>> + Send;

    /// Get the local address this listener is bound to.
    fn local_addr(&self) -> io::Result<String>;
}
```

The associated types `TcpStream` and `TcpListener` let each implementation provide its own concrete types. Production gives you `tokio::net::TcpStream`. Simulation gives you an in-memory stream backed by buffers with controllable latency, reordering, and connection failures.

The API deliberately matches what you would expect from tokio networking. `bind`, `connect`, `accept` behave like their tokio counterparts. The streams implement `AsyncRead + AsyncWrite + Send`, so they work with any tokio-compatible codec or framing layer **and** they cross task boundaries cleanly.

**Production**: `TokioNetworkProvider` wraps `tokio::net`.

**Simulation**: `NetworkSimulation` owns listeners, connections, topology,
faults, pending operation results, and network wakers. Bind, connect, and accept
park until their scheduled latency expires. Established streams use in-memory
buffers with deterministic delivery delays, TCP half-close simulation, and
fault injection such as connection drops, partitions, and corruption.

## TaskProvider

```rust
pub trait TaskProvider: Clone + Send + Sync + 'static {
    /// Join handle returned by `spawn_task`. `Detach` provides explicit
    /// fire-and-forget: `spawn_task(...).detach()` leaves the task running
    /// without keeping the handle.
    type JoinHandle: Future<Output = Result<(), JoinError>> + Detach + Send + 'static;

    /// Spawn a named task.
    fn spawn_task<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + Send + 'static;

    /// Yield control to allow other tasks to run.
    fn yield_now(&self) -> impl Future<Output = ()> + Send;
}
```

Spawned futures are **`Send + 'static`**. The runtime still pins everything to one OS thread for determinism, but the bound matches what `tokio::spawn` expects, so customer code reads exactly like normal tokio code. The `name` parameter is diagnostic only. In simulation the executor stores it with the task and traces every poll under it; in production `TokioTaskProvider` merely emits a `tracing::trace!` event when the task starts and when it completes.

**Production**: `TokioTaskProvider` uses plain `tokio::spawn`. The name feeds those two trace events and nothing else — it is not attached to the tokio task.

**Simulation**: `SimTaskProvider` spawns onto the [deterministic executor](./11-executor.md), so scheduling order is a seeded-random, fully reproducible function of the iteration seed.

## RandomProvider

```rust
pub trait RandomProvider: Clone + Send + Sync + 'static {
    /// Generate a random value of type T.
    fn random<T>(&self) -> T
    where
        StandardUniform: Distribution<T>;

    /// Generate a random value within a specified range (start..end).
    fn random_range<T>(&self, range: Range<T>) -> T
    where
        T: SampleUniform + PartialOrd;

    /// Generate a random f64 between 0.0 and 1.0.
    fn random_ratio(&self) -> f64 {
        self.random()
    }

    /// Generate a random bool with the given probability of being true.
    fn random_bool(&self, probability: f64) -> bool {
        self.random_ratio() < probability
    }
}
```

Only `random` and `random_range` are required; `random_ratio` and `random_bool` are default methods derived from them.

`RandomProvider` is fully synchronous. The other four providers expose async methods via native AFIT, but random number generation never needs to suspend, so its trait has no `async fn` at all. The supertrait shape (`Clone + Send + Sync + 'static`) stays consistent with the rest of the provider family.

**Production**: `TokioRandomProvider` uses `rand::rng()` (thread-local, non-deterministic).

**Simulation**: Uses the seeded `ChaCha8Rng` from the simulation's RNG system. Every call draws from the same deterministic stream, maintaining reproducibility.

## StorageProvider

```rust
pub trait StorageProvider: Clone + Send + Sync + 'static {
    type File: StorageFile + 'static;

    fn open(
        &self,
        path: &str,
        options: OpenOptions,
    ) -> impl Future<Output = io::Result<Self::File>> + Send;

    fn exists(&self, path: &str) -> impl Future<Output = io::Result<bool>> + Send;
    fn delete(&self, path: &str) -> impl Future<Output = io::Result<()>> + Send;
    fn rename(
        &self,
        from: &str,
        to: &str,
    ) -> impl Future<Output = io::Result<()>> + Send;
}

pub trait StorageFile: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + Sync + 'static {
    fn sync_all(&self) -> impl Future<Output = io::Result<()>> + Send;
    fn sync_data(&self) -> impl Future<Output = io::Result<()>> + Send;
    fn size(&self) -> impl Future<Output = io::Result<u64>> + Send;
    fn set_len(&self, size: u64) -> impl Future<Output = io::Result<()>> + Send;
}
```

Storage is the newest provider, and the one with the richest fault model. `OpenOptions` mirrors `std::fs::OpenOptions` with `read`, `write`, `create`, `truncate`, and `append` flags.

**Production**: `TokioStorageProvider` wraps `tokio::fs`.

**Simulation**: `StorageEngine` owns an in-memory filesystem with fault
injection inspired by TigerBeetle and FoundationDB patterns: read and write
corruption, crash and torn writes, misdirected I/O, sync failures, and
IOPS/bandwidth timing. Persistent file contents are separate from open-handle
state, so two handles have independent cursors, access options, and pending
operations. Each delayed operation has an exact ID, explicit pending or
completed result, and its own waker. Crash and shutdown complete pending work
with errors rather than treating absence as success.

Each `SimStorageProvider` is scoped to a process IP
(`SimStorageProvider::new(sim, ip)`), so the engine resolves the correct
per-process configuration and disk-degradation episode for every operation.

## The Providers Bundle

All five come together in the `Providers` trait:

```rust
pub trait Providers: Clone + Send + Sync + 'static {
    type Network: NetworkProvider;
    type Time: TimeProvider;
    type Task: TaskProvider;
    type Random: RandomProvider;
    type Storage: StorageProvider;

    fn network(&self) -> &Self::Network;
    fn time(&self) -> &Self::Time;
    fn task(&self) -> &Self::Task;
    fn random(&self) -> &Self::Random;
    fn storage(&self) -> &Self::Storage;
}
```

`TokioProviders` bundles all five production implementations. `SimProviders` bundles all five simulation implementations and requires an IP address at construction (`SimProviders::new(sim, seed, ip)`) so that the storage provider is scoped to the correct process. Your application code sees `P: Providers` and nothing else.
