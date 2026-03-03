# Quick Start: Swapping Implementations

<!-- toc -->

Here is what using providers looks like in practice. We will write a function that uses time and network providers, then show it running in both production and simulation contexts.

## A Function Generic Over Providers

```rust
use moonpool_core::{Providers, TimeProvider, NetworkProvider};
use std::time::Duration;

/// Connect to a peer and retry with exponential backoff.
async fn connect_with_retry<P: Providers>(
    providers: &P,
    addr: &str,
    max_retries: u32,
) -> std::io::Result<P::Network::TcpStream> {
    let mut delay = Duration::from_millis(100);

    for attempt in 0..max_retries {
        match providers.network().connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(e) if attempt + 1 < max_retries => {
                // Backoff before retrying — uses provider, not tokio directly
                providers.time().sleep(delay).await.ok();
                delay *= 2;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}
```

Notice what is **not** in this code: no `tokio::time::sleep()`. No `tokio::net::TcpStream::connect()`. The function uses `providers.time().sleep()` and `providers.network().connect()`. That is the entire discipline.

## The Forbidden List

These direct tokio calls break determinism. Never use them in application code:

| Forbidden | Use instead |
|-----------|-------------|
| `tokio::time::sleep()` | `providers.time().sleep()` |
| `tokio::time::timeout()` | `providers.time().timeout()` |
| `tokio::spawn()` | `providers.task().spawn_task()` |
| `tokio::net::TcpStream::connect()` | `providers.network().connect()` |
| `tokio::net::TcpListener::bind()` | `providers.network().bind()` |
| `tokio::fs::*` | `providers.storage().open()` / `exists()` / etc. |
| `rand::rng()` | `providers.random().random()` |

Any direct tokio call in your application code is a hole in the simulation. The call will use real I/O, real time, and the simulation has no control over it. The result is non-determinism: different behavior between runs with the same seed.

## Running in Production

```rust
use moonpool_core::TokioProviders;

let providers = TokioProviders::new();

// Real TCP connection, real exponential backoff with wall-clock delays
let stream = connect_with_retry(&providers, "10.0.1.1:9000", 5).await?;
```

`TokioProviders` bundles `TokioTimeProvider`, `TokioNetworkProvider`, `TokioTaskProvider`, `TokioRandomProvider`, and `TokioStorageProvider`. Each one delegates to the real tokio equivalent.

## Running in Simulation

Inside a simulation workload, the builder gives you `SimProviders`:

```rust
// The simulation provides SimProviders to your workload
// SimProviders bundles simulated time, network, tasks, random, and storage

let stream = connect_with_retry(&providers, "10.0.1.1:9000", 5).await?;
```

Same function call. But now `sleep()` advances simulation time instead of wall-clock time. `connect()` goes through the simulated network where connections can be delayed, dropped, or partitioned. The retry loop exercises the exact same code path, but under controlled, deterministic conditions.

That is the entire provider workflow: write your code generic over `P: Providers`, use provider methods instead of raw tokio, and the framework handles the rest.
