# Using Providers in Production
<!-- toc -->

Every chapter so far has been about testing. The point of writing against the
provider traits is that the code you tested is the code you ship. There is no
separate production version to keep in sync and no `#[cfg(test)]` fork in
behavior. The function that ran ten thousand seeds under simulation is the
function that now handles real traffic.
[Same Code in Production and Simulation](../part2-foundations/12-same-code.md)
states the rule. This chapter shows how to deploy under it.

## The production backend

Simulation hands your code `SimProviders`. Production hands it
`TokioProviders`, a bundle of the six real implementations: tokio tasks,
wall-clock time, real TCP, OS randomness, real files, and the kernel's CPU and
disk counters through `SystemProvider`. Code generic over `P: Providers`
accepts either one.

```rust
use moonpool::prelude::*;

// The same signature the tests use.
async fn fetch_with_retry<P: Providers>(providers: &P, max_attempts: u32)
    -> Result<u32, String>
{
    let time = providers.time();
    let random = providers.random();
    for attempt in 0..max_attempts {
        if random.random_bool(0.6) {
            return Ok(attempt);
        }
        // Every sleep goes through the provider, so simulation advances
        // logical time instead of waiting.
        let base_ms = 10u64 << attempt;
        let jitter_ms = random.random_range(0..base_ms.max(1));
        time.sleep(Duration::from_millis(base_ms + jitter_ms))
            .await
            .map_err(|e| e.to_string())?;
    }
    Err(format!("gave up after {max_attempts} attempts"))
}

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

The runnable version is
[`crates/moonpool/examples/retrying_worker.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool/examples/retrying_worker.rs).
Its `main` runs on tokio. Its `#[test]` drives the same `fetch_with_retry`
through `SimulationBuilder` across 50 seeds and asserts `failed_runs == 0`.
One function, two worlds: `cargo run --example retrying_worker` and
`cargo test --example retrying_worker`.

Networking follows the same swap. Code written against `NetworkProvider` sees
`SimTcpStream` in a simulation and a real tokio stream in production. An HTTP
or gRPC stack gets the same treatment through `moonpool-hyper`: `H2Server` and
`ReconnectingChannel` take `&TokioProviders` in production exactly as they take
`ctx.providers()` in a test.

## A lean dependency tree

The simulation runtime and the fork-based explorer have no place in a
production binary. The explorer pulls in `libc`, `fork` and `mmap`, machinery
that exists only to branch timelines during a test. A production build should
never compile it.

Moonpool keeps it out with features. The default (`sim` plus `tokio`) is the
full framework, which is what tests use. Production opts out:

```toml
[dependencies]
moonpool = { version = "0.8", default-features = false, features = ["tokio"] }
```

That stanza gives you the provider traits, `TokioProviders` and
`moonpool::select!` (tokio's macro, verbatim, since no sim is linked). Nothing
else. `cargo tree -e no-dev` on such a build shows no `moonpool-sim`, no
`moonpool-explorer`, no `moonpool-assertions`, no `hyper`, no `prometheus`. A
CI job runs that `cargo tree` on every change and fails the build if any of
them creeps back in.

| Feature | Pulls in | Use it when |
|---------|----------|-------------|
| `tokio` | `TokioProviders`, `moonpool::select!` | The application runs on real time, tasks, TCP, randomness and files |
| `hyper` | `moonpool::hyper`: runtime adapters, `HyperIo`, `H2Server`, `ReconnectingChannel` | The application uses hyper, axum or tonic |
| `prometheus` | `moonpool::prometheus`: `PrometheusSource` and instrumented handles | A `prometheus::Registry` you want in the simulation report |
| `rpc` | `moonpool::rpc`, typed dynamic-endpoint RPC | Processes call each other through moonpool-rpc |
| `rpc-derive` | the `#[moonpool_rpc::service]` attribute | Generated service interfaces |
| `rpc-tls` | rustls session upgrade (native only) | Server-authenticated RPC sessions |
| `rpc-jwt` | JWT and JWKS verification (native only) | Per-request credential checks |
| `sim` | the simulation runtime and the explorer | Tests, benchmarks, local runs (default) |

Keep `sim` on in `[dev-dependencies]` and off in the release build. The tests
run the full simulator, the shipped binary stays lean.

## Fault sites ship, and do nothing

Shipped code names its interesting moments. A buggify site marks a rare but
legal path. A hint marks a moment where a crash would be interesting. A CPU
charge names work that costs time. All three come from `moonpool-buggify`, a
crate with zero dependencies, so production code depends on it directly:

```toml
moonpool-buggify = "0.8"
```

```rust
use moonpool_buggify::{buggify, cpu_charge, hint};

async fn commit(batch: &Batch) -> io::Result<()> {
    storage.write_all(batch.bytes()).await?;
    storage.sync_all().await?;

    // Durable but not acknowledged. A crash here is the interesting one.
    hint!("batch durable, not acked").await;

    if buggify!() {
        // A legal slow path: pretend the ack was delayed.
        time.sleep(Duration::from_millis(50)).await?;
    }
    cpu_charge(Duration::from_micros(2) * batch.len() as u32);
    send_ack().await
}
```

In production every one of these is a **no-op**. `buggify!` returns `false`
because no random source is installed. `hint!` returns a future that resolves
at once because no sink is installed. `cpu_charge` reads one thread-local and
returns. Removing every site leaves the binary unchanged.

In simulation, `moonpool-sim` installs its seeded random source at the start
of each run, and its `HintSink` decides what a fired hint does: nothing, a
survivable fault, or a kill of the owning process, under the seed's chaos
regime, swarm mask and attrition budget. The code names the moment. The
simulator chooses the fault. [Buggify](../part3-building/08-buggify.md) and
[hint!](../part3-building/29-hints.md) cover both sides.

## One gotcha: `now()` is a stopwatch

In simulation, `time().now()` returns the canonical simulation clock. In
production, `TokioTimeProvider::now()` returns elapsed time since the provider
was **created**. It is a monotonic stopwatch, right for measuring durations and
scheduling relative deadlines, and deliberately not a calendar. For a
wall-clock timestamp in a log line or a persisted record, call
`std::time::SystemTime` at that call site. Keep sleeps, timeouts and backoff on
the provider so they stay testable. `time().timer()` is the application
clock: equal to `now()` in production, drifted by clock-skew faults in
simulation.

## Securing RPC in production

The simulation runs moonpool-rpc with scripted trust: a `FixedUtc` clock and
the `accept_credentials_over_plaintext` and `send_credentials_over_plaintext`
opt-ins, so it replays every decision without certificates. Production flips
each choice:

1. Give the server `SecurityConfig::enforced(verifier).with_clock(SystemUtc)`.
   The default `NoUtc` clock refuses every expiring credential.
2. Enable `rpc-tls` and wrap the transport in the session upgrade, so clients
   authenticate the server before a credential leaves.
3. Leave both plaintext opt-ins off.

A deployment that really trusts its network says so with
`SecurityConfig::trusted_network()`.
[Admission, Security and Derive](../part4-networking/04-rpc-operations.md)
has the details.

## Where it runs

The provider traits and the production backend compile broadly. The sim
runtime is portable too, with one boundary: the fork-based explorer needs a
POSIX process model.

| Target | core traits | `TokioProviders` | sim runtime | explorer (fork) |
|--------|:---:|:---:|:---:|:---:|
| Linux | yes | yes | yes | yes |
| macOS | yes | yes | yes | yes |
| `wasm32-unknown-unknown` | yes | no | yes, `--no-default-features` | no |

The simulation engine compiles to `wasm32-unknown-unknown` because it derives
everything from a seeded RNG, a logical clock and a cooperative scheduler. The
explorer cannot follow: it is built on `fork()`, `waitpid()` and `MAP_SHARED`
memory, which Linux and macOS provide (CI runs the exploration suite on both).
On wasm the simulator runs with the in-process assertion table. You lose
multiverse forking, not correctness checking.
[Simulation in the Browser](./07-wasm.md) shows it running in a tab.
