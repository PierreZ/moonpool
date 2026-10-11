# Quick Start: Swapping Implementations

<!-- toc -->

Here is what providers look like in practice. We write one function that uses the time and random providers, run it on tokio, then run the **same function** under simulation. The code is adapted from `crates/moonpool/examples/retrying_worker.rs`, which you can run both ways.

## A Function Generic Over Providers

```rust,ignore
use moonpool::prelude::*;
use std::time::Duration;

/// Retry with exponential backoff and jitter, through providers only.
async fn fetch_with_retry<P: Providers>(providers: &P, max_attempts: u32) -> Result<u32, String> {
    let time = providers.time();
    let random = providers.random();

    for attempt in 0..max_attempts {
        // Stand-in for a fallible network call: succeeds 60% of the time.
        // In the sim this coin comes from the seeded stream.
        if random.random_bool(0.6) {
            return Ok(attempt);
        }

        // Every sleep goes through the time provider, so the simulation
        // advances logical time instead of waiting.
        let base_ms = 10u64 << attempt;
        let jitter_ms = random.random_range(0..base_ms.max(1));
        time.sleep(Duration::from_millis(base_ms + jitter_ms))
            .await
            .map_err(|e| e.to_string())?;
    }

    Err(format!("gave up after {max_attempts} attempts"))
}
```

Notice what is **not** in this code: no `tokio::time::sleep()`, no `rand::rng()`. The function uses `providers.time().sleep()` and `providers.random().random_bool()`. That is the whole discipline. `moonpool::prelude::*` brings the provider traits into scope, which you need because their `async fn`s are only callable with the trait imported.

## The Forbidden List

These direct calls break determinism. Never use them in code that will run under simulation:

| Forbidden | Use instead |
|-----------|-------------|
| `tokio::time::sleep()` | `providers.time().sleep()` |
| `tokio::time::timeout()` | `providers.time().timeout()` |
| `std::time::Instant::now()` | `providers.time().now()` |
| `tokio::spawn()` | `providers.task().spawn_task("name", fut)` |
| `tokio::task::yield_now()` | `providers.task().yield_now()` |
| `tokio::select!` | `moonpool::select!` (seeded branch offset) |
| `tokio::net::TcpStream::connect()` | `providers.network().connect()` |
| `tokio::net::TcpListener::bind()` | `providers.network().bind()` |
| `tokio::net::lookup_host()` | a `Resolver` |
| `tokio::fs::*` | `providers.storage().open()` and friends |
| `rand::rng()` | `providers.random().random()` |
| `getrusage`, `/proc/diskstats` | `providers.system().sample(data_dir)` |

A direct tokio call is a hole in the simulation. It uses real I/O or real time, and the simulation has no control over it. The result is two different runs for one seed. Inside a simulation a bare `tokio::spawn` does not even run: there is no tokio runtime, so it panics.

`tokio::select!` is the subtle one. Its fair mode draws a start offset from tokio's thread-local RNG, which is seeded from process entropy outside a tokio runtime. `moonpool::select!` is tokio's own macro expansion with the offset drawn from the simulation stream, so the grammar is identical and only the number source changes. Outside a simulation it behaves like tokio's. [The Six Providers](./07-provider-traits.md#select) has the details.

## Running in Production

```rust,ignore
use moonpool::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let providers = TokioProviders::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(fetch_with_retry(&providers, 5));
    println!("production run finished: {result:?}");
    Ok(())
}
```

`TokioProviders` bundles `TokioTimeProvider`, `TokioTaskProvider`, `TokioNetworkProvider`, `TokioRandomProvider`, `TokioStorageProvider` and `TokioSystemProvider`. Each one delegates to tokio, `rand` or the kernel. Real sleeps, real sockets, OS entropy.

## Running in Simulation

A workload pulls `SimProviders` from its context and hands it to the same function:

```rust,ignore
use async_trait::async_trait;
use moonpool::{SimContext, SimulationBuilder, SimulationResult, Workload};

struct RetryWorkload;

#[async_trait]
impl Workload for RetryWorkload {
    fn name(&self) -> &'static str {
        "retry-client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Either outcome is fine: we assert the worker never panics or hangs
        // under simulated time, across many seeds.
        let _ = fetch_with_retry(ctx.providers(), 5).await;
        Ok(())
    }
}

#[test]
fn survives_chaos() {
    let report = SimulationBuilder::new()
        .set_iterations(50)
        .workload(RetryWorkload)
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "worker should survive every seed");
}
```

Same function call. Now `sleep()` advances simulation time, so 50 seeds with their backoff schedules finish in milliseconds. `random_bool(0.6)` draws from the seeded stream, so a failing seed replays the same sequence of outcomes. Run both from the example:

```text
cargo run --example retrying_worker    # production, on tokio
cargo test --example retrying_worker   # simulation, 50 seeds
```

That is the whole workflow. Write code generic over `P: Providers`, call provider methods instead of tokio, and let the runner pick the implementation.
