# Using Providers in Production

<!-- toc -->

Every chapter so far has been about testing. But the whole point of writing
against the provider traits is that the code you tested is the code you ship.
There is no separate "production version" to keep in sync, no `#[cfg(test)]`
fork in behavior. The function you ran ten million times under simulation is the
exact function that now handles real traffic.

So how do we actually deploy it?

## The production backend

Simulation swaps in `SimProviders`. Production swaps in `TokioProviders` — a
zero-cost bundle of the five real implementations: wall-clock time, tokio task
spawning, real TCP, OS randomness, and real filesystem I/O. Code that is generic
over `P: Providers` accepts either one.

```rust
use moonpool::prelude::*;

// The same signature you used in tests.
async fn fetch_with_retry<P: Providers>(providers: &P, max_attempts: u32)
    -> Result<u32, String>
{
    // time().sleep(), random().random_bool(), task().spawn_task() ...
}

#[tokio::main]
async fn main() {
    let providers = TokioProviders::new();
    let result = fetch_with_retry(&providers, 5).await;
    println!("{result:?}");
}
```

The complete, runnable version lives in
[`moonpool/examples/retrying_worker.rs`](https://github.com/PierreZ/moonpool/blob/main/moonpool/examples/retrying_worker.rs).
Its `main` runs on Tokio; its `#[test]` drives the same `fetch_with_retry`
through `SimulationBuilder` across 50 seeds. One function, two worlds, verified
by `cargo test --example retrying_worker`.

For the transport layer the production path is just as short. The `tokio` feature
gives you a `TokioTransport` alias and a builder shortcut, so you skip naming the
providers bundle:

```rust
let transport = NetTransportBuilder::tokio()
    .local_address(NetworkAddress::parse("127.0.0.1:4500")?)
    .build_listening()
    .await?;
```

## A lean dependency tree

The simulation runtime and the fork-based explorer have no business in a
production binary. The explorer in particular pulls in `libc`, `fork`, and
`mmap` — machinery that exists only to branch timelines during testing. A
production build should never compile it.

Moonpool keeps it out with features. The default pulls the whole framework
(that is what the crate's audience uses day to day). Production opts out:

```toml
[dependencies]
moonpool = { version = "0.8", default-features = false, features = ["tokio", "transport"] }
```

That stanza gives you the provider contract, the production backend, and the
transport layer — and nothing else. `cargo tree` on such a build shows no
`moonpool-sim`, no `moonpool-explorer`, no `moonpool-assertions`. The only system
libraries present are the ones tokio itself needs for real sockets and files.

| Feature | Pulls in | Use it when |
|---------|----------|-------------|
| `tokio` | `TokioProviders`, `TokioTransport` | Always, in production |
| `transport` | `NetTransport`, `#[service]` RPC | You speak the moonpool wire protocol |
| `sim` | the simulation runtime + explorer | Tests, benchmarks, local dev (default) |

Keep `sim` on as a `dev-dependency` feature and off in your release profile, and
your tests still run the full simulator while your shipped binary stays lean.

## One gotcha: `TokioTimeProvider::now()`

In simulation, `time().now()` returns the canonical simulation clock. In
production, `TokioTimeProvider::now()` returns elapsed time since the provider
was **created**, not wall-clock time. It is a monotonic stopwatch, perfect for
measuring durations and scheduling relative deadlines, and deliberately not a
calendar. If you need a wall-clock timestamp for logging or persistence, reach
for `std::time::SystemTime` directly at that call site. Everything that drives
sleeps, timeouts, and backoff should keep going through the provider so it stays
testable.

## Where it runs

The provider contract and the production backend compile broadly. The sim
runtime is portable too, with one boundary: the fork-based explorer is POSIX and
Linux-first.

| Target | core + tokio | transport | sim runtime | explorer (fork) |
|--------|:---:|:---:|:---:|:---:|
| Linux | yes | yes | yes | yes |
| macOS | yes | yes | yes | best-effort |
| `wasm32-unknown-unknown` | task/time only | no (no sockets) | yes, `--no-default-features` | no |

The simulation engine compiles to `wasm32-unknown-unknown` because it derives
everything from a seeded RNG, a logical clock, and a cooperative scheduler — no
operating system required. The explorer cannot follow it there, and on macOS its
`fork`-without-`exec` model is fragile, so treat exploration as a Linux
amplifier. Everywhere else the full simulator runs via the in-process assertion
table; you lose multiverse forking, not correctness checking. If a build refuses
to include the explorer, the fallback is one line:
`moonpool-sim = { default-features = false }`.
