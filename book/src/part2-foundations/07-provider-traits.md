# The Five Providers

<!-- toc -->

Moonpool abstracts every interaction between your code and the outside world into five provider traits. Each trait covers one category of I/O. Together, they form a complete boundary around your application, giving the simulator full control over every source of non-determinism.

## TimeProvider

Time is the most pervasive dependency in distributed systems. Every timeout, backoff, heartbeat, and lease check goes through `TimeProvider`.

```rust
#[async_trait(?Send)]
pub trait TimeProvider: Clone {
    /// Sleep for the specified duration.
    async fn sleep(&self, duration: Duration) -> Result<(), TimeError>;

    /// Get exact current time.
    fn now(&self) -> Duration;

    /// Get drifted timer time (simulates clock drift between nodes).
    fn timer(&self) -> Duration;

    /// Run a future with a timeout.
    async fn timeout<F, T>(&self, duration: Duration, future: F) -> Result<T, TimeError>
    where
        F: std::future::Future<Output = T>;
}
```

The distinction between `now()` and `timer()` is borrowed from FoundationDB's `sim2`. In production, both return the same value. In simulation, `timer()` can drift up to 100ms ahead of `now()`, testing how your code handles clock skew between processes. Use `now()` for event scheduling. Use `timer()` for application-level time checks like lease expiry and heartbeat deadlines.

**Production**: `TokioTimeProvider` delegates `sleep` to `tokio::time::sleep`, `timeout` to `tokio::time::timeout`, and `now` to `std::time::Instant::elapsed`.

**Simulation**: Sleep schedules an event on the simulation event queue. When all tasks are blocked, the simulator performs "time travel," jumping forward to the next scheduled event. This compresses hours of simulated cluster time into seconds of wall-clock time.

## NetworkProvider

```rust
#[async_trait(?Send)]
pub trait NetworkProvider: Clone {
    type TcpStream: AsyncRead + AsyncWrite + Unpin + 'static;
    type TcpListener: TcpListenerTrait<TcpStream = Self::TcpStream> + 'static;

    /// Create a TCP listener bound to the given address.
    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener>;

    /// Connect to a remote address.
    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream>;
}

#[async_trait(?Send)]
pub trait TcpListenerTrait {
    type TcpStream: AsyncRead + AsyncWrite + Unpin + 'static;

    /// Accept a single incoming connection.
    async fn accept(&self) -> io::Result<(Self::TcpStream, String)>;

    /// Get the local address this listener is bound to.
    fn local_addr(&self) -> io::Result<String>;
}
```

The associated types `TcpStream` and `TcpListener` let each implementation provide its own concrete types. Production gives you `tokio::net::TcpStream`. Simulation gives you an in-memory stream backed by buffers with controllable latency, reordering, and connection failures.

The API deliberately matches what you would expect from tokio networking. `bind`, `connect`, `accept` behave like their tokio counterparts. The streams implement `AsyncRead + AsyncWrite`, so they work with any tokio-compatible codec or framing layer.

**Production**: `TokioNetworkProvider` wraps `tokio::net`.

**Simulation**: Connections are in-memory buffer pairs with deterministic delivery delays, TCP half-close simulation, and fault injection (connection drops, partitions, delayed delivery).

## TaskProvider

```rust
#[async_trait(?Send)]
pub trait TaskProvider: Clone {
    /// Spawn a named task that runs on the current thread.
    fn spawn_task<F>(&self, name: &str, future: F) -> tokio::task::JoinHandle<()>
    where
        F: Future<Output = ()> + 'static;

    /// Yield control to allow other tasks to run.
    async fn yield_now(&self);
}
```

Tasks are always local (no `Send` bound on `F`). The `name` parameter is used for tracing and debugging. In simulation, it shows up in event logs so you can trace which task generated which event.

**Production**: `TokioTaskProvider` uses `tokio::task::spawn_local`.

**Simulation**: The simulator controls task scheduling order, making it deterministic and seed-dependent.

## RandomProvider

```rust
pub trait RandomProvider: Clone {
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

`RandomProvider` is the only provider without `#[async_trait(?Send)]` because random number generation is synchronous.

**Production**: `TokioRandomProvider` uses `rand::rng()` (thread-local, non-deterministic).

**Simulation**: Uses the seeded `ChaCha8Rng` from the simulation's RNG system. Every call draws from the same deterministic stream, maintaining reproducibility.

## StorageProvider

```rust
#[async_trait(?Send)]
pub trait StorageProvider: Clone {
    type File: StorageFile + 'static;

    async fn open(&self, path: &str, options: OpenOptions) -> io::Result<Self::File>;
    async fn exists(&self, path: &str) -> io::Result<bool>;
    async fn delete(&self, path: &str) -> io::Result<()>;
    async fn rename(&self, from: &str, to: &str) -> io::Result<()>;
}

#[async_trait(?Send)]
pub trait StorageFile: AsyncRead + AsyncWrite + AsyncSeek + Unpin {
    async fn sync_all(&self) -> io::Result<()>;
    async fn sync_data(&self) -> io::Result<()>;
    async fn size(&self) -> io::Result<u64>;
    async fn set_len(&self, size: u64) -> io::Result<()>;
}
```

Storage is the newest provider, and the one with the richest fault model. `OpenOptions` mirrors `std::fs::OpenOptions` with `read`, `write`, `create`, `truncate`, and `append` flags.

**Production**: `TokioStorageProvider` wraps `tokio::fs`.

**Simulation**: In-memory filesystem with fault injection inspired by TigerBeetle and FoundationDB patterns: read/write corruption, crash and torn writes, misdirected reads/writes, sync failures, and IOPS/bandwidth timing simulation. Each `SimStorageProvider` is scoped to a process IP (`SimStorageProvider::new(sim, ip)`), and files are tagged with `owner_ip` so fault injection uses the correct per-process configuration.

## The Providers Bundle

All five come together in the `Providers` trait:

```rust
pub trait Providers: Clone + 'static {
    type Network: NetworkProvider + Clone + 'static;
    type Time: TimeProvider + Clone + 'static;
    type Task: TaskProvider + Clone + 'static;
    type Random: RandomProvider + Clone + 'static;
    type Storage: StorageProvider + Clone + 'static;

    fn network(&self) -> &Self::Network;
    fn time(&self) -> &Self::Time;
    fn task(&self) -> &Self::Task;
    fn random(&self) -> &Self::Random;
    fn storage(&self) -> &Self::Storage;
}
```

`TokioProviders` bundles all five production implementations. `SimProviders` bundles all five simulation implementations and requires an IP address at construction (`SimProviders::new(sim, seed, ip)`) so that the storage provider is scoped to the correct process. Your application code sees `P: Providers` and nothing else.
