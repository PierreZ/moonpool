# Migrating Existing Code to Providers
<!-- toc -->

You have a service that already works. It sleeps, it spawns, it opens sockets
and files, and somewhere it calls `rand`. None of that runs under simulation
yet, because every one of those calls reaches straight into the operating
system and the wall clock. The job is to route each of them through a provider,
so that the same function can one day run under `SimProviders` and replay from
a seed.

This is a refactor, not a rewrite. The code keeps running on real tokio the
whole time, because `TokioProviders` wraps the exact primitives you replace. You
do not change behavior. You change **who you ask** for time, tasks, randomness,
sockets, files and system counters.

## Thread one bundle from the top

Before you touch a call site, decide how the providers reach your code. The
pattern is one type parameter, `P: Providers`, threaded from `main` down the
call graph. Build the bundle once at the top and pass it by reference.

```rust
use moonpool::prelude::*;

async fn run<P: Providers>(providers: &P) -> Result<(), MyError> {
    // everything non-deterministic goes through `providers`
}

#[tokio::main]
async fn main() -> Result<(), MyError> {
    let providers = TokioProviders::new();
    run(&providers).await
}
```

The discipline is to **resist the shortcut**. Do not call `tokio::time::sleep`
three layers deep because the provider is not in scope. Plumb it through. A
function that still reaches for a global is a function the simulation cannot
control.

## The call mapping

Most of the migration is mechanical. Each primitive has a direct provider
equivalent.

| You have | Swap to |
|----------|---------|
| `tokio::time::sleep(d).await` | `providers.time().sleep(d).await?` |
| `tokio::time::timeout(d, fut).await` | `providers.time().timeout(d, fut).await` |
| `Instant::now()` for elapsed time | `providers.time().now()` (a stopwatch, see below) |
| a lease or heartbeat clock | `providers.time().timer()` (drifts under simulation) |
| `tokio::spawn(fut)` | `providers.task().spawn_task("name", fut)` |
| `tokio::task::yield_now().await` | `providers.task().yield_now().await` |
| `tokio::select! { … }` | `moonpool::select! { … }` |
| `rand::random()`, `rng().random()` | `providers.random().random()` |
| `rng().random_range(a..b)` | `providers.random().random_range(a..b)` |
| `rng().random_bool(p)` | `providers.random().random_bool(p)` |
| `tokio::fs` / `std::fs` | `providers.storage().open(path, OpenOptions)` plus `exists`, `delete`, `rename`, `sync_dir` |
| `tokio::net::TcpListener::bind(addr)` | `providers.network().bind(addr).await` |
| `tokio::net::TcpStream::connect(addr)` | `providers.network().connect(addr).await` |
| `getrusage`, `/proc/stat`, `/proc/diskstats` | `providers.system().sample(data_dir)` |

Two type changes ride along. `time().sleep` returns `Result<(), TimeError>`,
so add a `?`. `time().timeout` returns `Result<T, TimeError>` instead of
tokio's `Result<T, Elapsed>`, so match on `TimeError::Elapsed`.

`moonpool::select!` matters more than it looks. tokio's macro picks a random
start branch from a thread-local RNG that no seed controls. moonpool's macro is
tokio's expansion driven by a seeded offset when a simulation is linked, and
tokio's macro verbatim in the lean production build. Same syntax, including
`biased;`.

## Gotcha 1: the I/O traits move to `futures::io`

This one surprises people. The tokio network and storage providers wrap
`tokio::net::TcpStream` and `tokio::fs::File` in `tokio_util::compat::Compat`,
which exposes the runtime-agnostic `futures::io::{AsyncRead, AsyncWrite,
AsyncSeek}` traits instead of `tokio::io::*`. The socket is still a real socket.
Only the extension trait you import changes.

```rust
// Before: tokio's extension traits.
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// After: the same method names, from futures.
use futures::io::{AsyncReadExt, AsyncWriteExt};

stream.write_all(b"hello").await?;
let n = stream.read(&mut buf).await?;
```

`write_all`, `read`, `read_exact` and `read_to_end` all exist on the futures
traits. Seeking comes from `futures::io::AsyncSeekExt`. The tokio-only
`read_buf` and `AsyncBufRead` have no direct equivalent, so a read loop built
on them needs a small restructure into plain `read` calls. For hyper, axum and
tonic, `moonpool_hyper::HyperIo` does the adaptation
([The hyper Stack](./08-hyper-stack.md)).

## Gotcha 2: `now()` is a stopwatch, not a calendar

`providers.time().now()` returns elapsed time since the provider was created.
It is right for durations and relative deadlines and useless as a timestamp.
When a log line or a persisted record needs calendar time, call
`std::time::SystemTime` at that exact site and leave everything that drives
sleeps, timeouts and backoff on the provider.
[Using Providers in Production](./05-production.md) has more.

## Gotcha 3: replace every last one

One surviving `tokio::time::sleep` or `rand::rng()` is enough to make a future
simulation non-reproducible, and it will not announce itself. Make the sweep
exhaustive:

1. Grep the crate for `tokio::`, `rand::`, `std::fs`, `std::time::Instant`
   and `SystemTime`.
2. Confirm each remaining hit is outside the deterministic path.
3. Replace `HashMap` iteration that affects behavior with `BTreeMap`.
4. Run the existing tokio tests. They must still pass.

The compiler helps once the function is generic over `P: Providers`, because
the providers are the only async machinery in scope. Synchronous calls like
`Instant::now()` slip through type checking and need eyes.
[Sim Compatibility Checklist](../appendix/06-sim-compatibility.md) is the full
list.

## Name the moments while you are there

The migration pass is the right time to add the sites simulation will use
later. They cost nothing in production and come from `moonpool-buggify`, which
has no dependencies.

- `hint!("label").await` after a durable write that is not yet acknowledged,
  before a reply is sent, between two steps of a protocol. The simulator may
  crash the process there.
- `buggify!()` on a rare but legal path: an early timeout, a dropped buffer,
  a slow branch.
- `cpu_charge(duration)` on a hot path a profile shows: a batch encode, a
  checksum, a fold.

[Same Code in Production and Simulation](../part2-foundations/12-same-code.md)
explains why the sites live in shipped code and not in a test double.

## Knowing the swap was safe

Because `TokioProviders` is the production backend, "did I change behavior?"
has a concrete answer. Moonpool ships a conformance suite under
[`crates/moonpool-sim/tests/conformance/`](https://github.com/PierreZ/moonpool/tree/main/crates/moonpool-sim/tests/conformance).
One generic contract per provider (time, task, random, network, storage, and
the bundle) runs against `TokioProviders` on a real current-thread runtime. It
asserts the properties your migrated code depends on: a sleep advances the
clock by at least its duration, a timeout over a stalled future elapses, a
byte-for-byte echo survives a round trip, a write followed by `sync_all` reads
back identical, and a missing or existing file surfaces as `NotFound` or
`AlreadyExists`. The network contract also runs against `SimProviders` on the
deterministic executor, with the same body, which is how the two backends are
kept honest.

The loop is short. Make a function generic over `P: Providers`, swap its calls
using the table, fix the `futures::io` imports, and run your tokio tests. If
they pass, you lost no behavior and gained a function that simulation can
drive.
