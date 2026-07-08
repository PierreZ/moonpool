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
    fn timer(&self) -> Duration;

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

The distinction between `now()` and `timer()` is borrowed from FoundationDB's `sim2`. In production, both return the same value. In simulation, `timer()` can drift up to 100ms ahead of `now()`, testing how your code handles clock skew between processes. Use `now()` for event scheduling. Use `timer()` for application-level time checks like lease expiry and heartbeat deadlines.

**Production**: `TokioTimeProvider` delegates `sleep` to `tokio::time::sleep`, `timeout` to `tokio::time::timeout`, and `now` to `std::time::Instant::elapsed`.

**Simulation**: Sleep schedules an event on the simulation event queue. When all tasks are blocked, the simulator performs "time travel," jumping forward to the next scheduled event. This compresses hours of simulated cluster time into seconds of wall-clock time.

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

**Simulation**: Connections are in-memory buffer pairs with deterministic delivery delays, TCP half-close simulation, and fault injection (connection drops, partitions, delayed delivery).

## TaskProvider

```rust
pub trait TaskProvider: Clone + Send + Sync + 'static {
    /// Join handle returned by `spawn_task`.
    type JoinHandle: Future<Output = Result<(), JoinError>> + Send + Sync + 'static;

    /// Spawn a named task.
    fn spawn_task<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + Send + 'static;

    /// Yield control to allow other tasks to run.
    fn yield_now(&self) -> impl Future<Output = ()> + Send;
}
```

Spawned futures are **`Send + 'static`**. The runtime still pins everything to one OS thread for determinism, but the bound matches what `tokio::spawn` expects, so customer code reads exactly like normal tokio code. The `name` parameter shows up in event logs so you can trace which task generated which event.

**Production**: `TokioTaskProvider` uses `tokio::task::Builder::new().name(...).spawn(...)`, which is plain `tokio::spawn` with a name attached.

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
    fn random_ratio(&self) -> f64;

    /// Generate a random bool with the given probability of being true.
    fn random_bool(&self, probability: f64) -> bool;
}
```

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

**Simulation**: In-memory filesystem with fault injection inspired by TigerBeetle and FoundationDB patterns: read/write corruption, crash and torn writes, misdirected reads/writes, sync failures, and IOPS/bandwidth timing simulation. Each `SimStorageProvider` is scoped to a process IP (`SimStorageProvider::new(sim, ip)`), and files are tagged with `owner_ip` so fault injection uses the correct per-process configuration.

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
