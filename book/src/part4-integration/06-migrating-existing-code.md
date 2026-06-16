# Migrating Existing Code to Providers

<!-- toc -->

You have a service that already works. It sleeps, it spawns, it opens sockets and
files, and somewhere it calls `rand`. None of that is testable under simulation
yet, because every one of those calls reaches straight into the operating system
and the wall clock. The job is to route each of them through a provider instead,
so that one day the same function can run under `SimProviders` and replay
deterministically.

The reassuring part is that this is a refactor, not a rewrite. The code keeps
running on real Tokio the whole time, because `TokioProviders` wraps the exact
primitives you are replacing. You are not changing behavior, you are changing
**who you ask** for time, tasks, randomness, sockets, and files. Simulation comes
later. Today the only thing that has to be true is that `TokioProviders` behaves
like the raw calls, and it does.

## Thread one bundle from the top

Before touching any call site, decide how the provider reaches your code. The
pattern is a single type parameter, `P: Providers`, threaded from `main` down
through the call graph. Construct the bundle once at the top and pass it by
reference.

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

The discipline that makes this work is **resisting the shortcut**. Do not call
`tokio::time::sleep` three layers deep because the provider is not in scope.
Plumb the provider through. A function that still reaches for a global is a
function simulation cannot control.

## The call mapping

Most of the migration is mechanical. Each non-deterministic primitive has a
direct provider equivalent.

| You have | Swap to |
|----------|---------|
| `tokio::time::sleep(d).await` | `providers.time().sleep(d).await?` |
| `tokio::time::timeout(d, fut).await` | `providers.time().timeout(d, fut).await` |
| `Instant::now()` for measuring elapsed | `providers.time().now()` (a stopwatch, see below) |
| `tokio::spawn(fut)` | `providers.task().spawn_task("name", fut)` |
| `tokio::task::yield_now().await` | `providers.task().yield_now().await` |
| `rand::random()`, `thread_rng().random()` | `providers.random().random()` |
| `thread_rng().random_range(a..b)` | `providers.random().random_range(a..b)` |
| `tokio::fs` / `std::fs` | `providers.storage().open(path, OpenOptions)` plus `exists` / `delete` / `rename` |
| `tokio::net::TcpListener::bind(addr)` | `providers.network().bind(addr).await` |
| `tokio::net::TcpStream::connect(addr)` | `providers.network().connect(addr).await` |

Two small type changes ride along. `time().sleep` returns
`Result<(), TimeError>`, so add a `?`. `time().timeout` returns
`Result<T, TimeError>` rather than tokio's `Result<T, Elapsed>`, so match on the
moonpool error.

## Gotcha 1: the I/O traits move to `futures::io`

This is the one that surprises people. The Tokio network and storage providers
wrap their `tokio::net::TcpStream` and `tokio::fs::File` in
`tokio_util::compat::Compat`, which exposes the runtime-agnostic
`futures::io::{AsyncRead, AsyncWrite, AsyncSeek}` traits instead of
`tokio::io::*`. The stream is still a real socket and the file is still a real
file. Only the extension trait you import changes.

```rust
// Before: tokio's extension traits.
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// After: the same method names, from futures.
use futures::io::{AsyncReadExt, AsyncWriteExt};

stream.write_all(b"hello").await?;
let n = stream.read(&mut buf).await?;
```

`write_all`, `read`, `read_exact`, and `read_to_end` all live on the futures
traits, and seeking comes from `futures::io::AsyncSeekExt`. The Tokio-only
conveniences `read_buf` and `AsyncBufRead` have no direct equivalent, so a read
loop written around them needs a small restructure into plain `read` calls.

## Gotcha 2: `now()` is a stopwatch, not a calendar

`providers.time().now()` returns elapsed time since the provider was created, not
wall-clock time. It is perfect for measuring durations and relative deadlines and
deliberately useless as a timestamp. If a log line or a persisted record needs a
real calendar time, reach for `std::time::SystemTime` at that exact call site and
leave everything that drives sleeps, timeouts, and backoff going through the
provider. The
[Using Providers in Production](./05-production.md) chapter covers this in more
detail.

## Gotcha 3: replace every last one

A single surviving `tokio::time::sleep` or `thread_rng()` is enough to make a
future simulation non-reproducible, and it will not announce itself. Make the
sweep exhaustive. Grep the crate for `tokio::`, `rand::`, `std::fs`, and
`std::time::Instant`, and confirm each remaining hit is genuinely outside the
deterministic path. The compiler helps once the function is generic over
`P: Providers`, because the providers are the only async machinery in scope, but
synchronous calls like `Instant::now()` slip through type checking and need eyes.

## Knowing the swap was safe

Because `TokioProviders` is the production backend, "did I change behavior?" has a
concrete answer. Moonpool ships a conformance suite under
[`moonpool-sim/tests/conformance/`](https://github.com/PierreZ/moonpool/tree/main/moonpool-sim/tests/conformance)
that runs one generic contract per provider against `TokioProviders` on a real
runtime. It asserts the properties your migrated code depends on: a sleep advances
the clock by at least its duration, a timeout over a stalled future elapses, a
byte-for-byte echo survives a round trip, a write followed by `sync_all` reads
back identical, and missing or already-existing files surface as `NotFound` and
`AlreadyExists`. Each contract is written generically so the very same body will
later run against `SimProviders`, which is how the production and simulation
backends are kept honest.

In practice the loop is short. Make a function generic over `P: Providers`, swap
its non-deterministic calls using the table above, fix the `futures::io` imports,
and run your existing Tokio tests. If they still pass, you have lost no behavior
and gained a function that simulation can drive.
