# Same Code in Production and Simulation

<!-- toc -->

One rule sits under every other design choice in moonpool: **the code you ship is the code you simulate**. Not a copy of it. Not a wrapper around it. The same compiled function, with a different `Providers` type plugged in. This chapter states the rule, shows the three mechanisms that make it hold, and shows how a crate keeps the simulator out of its release binary.

## No Sim* Wrappers

The tempting shortcut is a test-only twin of each component: a `SimMachine` next to `Machine`, a `SimDisk` next to `Disk`, a `#[cfg(test)]` hook that lets the test reach inside. Each twin is a place where the tested behaviour and the shipped behaviour drift apart. After a year, the twin tests itself.

Moonpool has none of these in shipped code. There is no `SimMachine`, no `SimDisk`, no test-only hook inside a journal or an RPC runtime. The only `Sim*` types in the workspace are the **providers** themselves: `SimTimeProvider`, `SimNetworkProvider`, `SimStorageProvider` and their siblings in moonpool-sim. They implement the same traits `TokioTimeProvider` and `TokioStorageProvider` implement. Everything above the trait line is one implementation.

The same rule applies to your code. A process in a simulation is your server, built by a factory, not a simplified model of it. moonpool-journal is a worked example: one `Journal<P: StorageProvider>` runs as `Journal<TokioStorageProvider>` in its `accept_log` example and as `Journal<SimStorageProvider>` in its tests, and the crash tests exercise the exact commit path production uses.

## Three Mechanisms

Three things let one implementation serve both worlds.

**Providers carry the I/O.** The code depends on `P: Providers`, never on tokio. In production `main` builds `TokioProviders::new()`. In a simulation the runner hands a `SimProviders` to each process and workload through `ctx.providers()`. [The Provider Pattern](./04-provider-pattern.md) covers this. moonpool-rpc and moonpool-hyper follow the same rule: both are generic over `P: Providers`, ship no providers, and never depend on moonpool-sim.

**The code names moments, the simulator strikes.** Some faults are best placed by the code that knows the moment. A journal knows the instant where its entries are written but not synced. The code names that moment with `hint!` from moonpool-buggify:

```rust,ignore
segment.write(batch.entry_at, &batch.entries).await?;
// A power loss here tears entries no record points at yet.
moonpool_buggify::hint!("journal commit: entries written, not synced", COMMIT_HINT_PROB).await;
segment.sync().await?;
```

The code never names a fault. It says "this is interesting". In production the point draws nothing, asks no one, and the future resolves at once. In a simulation the point is a disruptive buggify site: activated once per run, firing at `POINT_PROB` (5%) per call or at the rate the site gives. When it fires, moonpool-sim's `HintSink` finds the calling process through the owner of the task being polled and asks the first attrition regime whose victim filter admits that process. The regime's `max_dead` budget, reboot kinds and recovery delays apply, as for its own timed reboots. The harness can add a `HintVeto` for a budget the simulator cannot see. A seed whose swarm drew the never-reboot regime never reboots on a hint. [hint!: Naming Fault Sites](../part3-building/29-hints.md) covers the vetoes.

**The code's own choices use buggify.** When the code itself picks a rare but legal behaviour, it uses the inline macros: `buggify!()` fires at 25% while its site is active, `buggify_with_prob!(p)` at a site rate, `buggify_fault_with_prob!(p)` for a site that makes an operation fail and goes silent in the recovery tail, `buggify_named!("label", p)` for a site a harness can switch per seed with `set_activation`, and `buggify_pick!` and `buggify_range!` for a site that overrides a choice or stretches a value. moonpool-rpc uses these inside its shipped transport: a session cut right after a frame, a request refused as `Overloaded`, a deferred stream acknowledgement. `cpu_charge(duration)` is the same idea for CPU time: shipped code reports that it did a known amount of work, and a simulator with a CPU model makes that work take time.

## Inert in Production

All of this costs nothing outside a simulation. moonpool-buggify is a pure-std crate with zero dependencies. Its state is thread-local and disabled by default. Outside a simulation:

- every `buggify!`, `buggify_with_prob!`, `buggify_fault_with_prob!` and `buggify_named!` site evaluates to `false` with no draw and no side effect
- `buggify_pick!` and `buggify_range!` return `None`
- `hint!` returns a future that resolves at once, without asking any sink
- `cpu_charge` does one thread-local read and returns
- `is_simulated()` returns `false`, so code can tilt a rate or a check in simulation without changing an outcome a caller can observe

A simulation turns the crate on for one run: moonpool-sim installs its seeded draw with `set_random_source`, calls `buggify_init(0.5)` so each site is active with 50% probability, installs its hint and CPU sinks, and resets everything when the run ends. The same thread-local state serves `moonpool_sim::buggify!` and `moonpool_buggify::buggify!`, so a library and the harness that tests it share one set of activation decisions.

## Keeping Sim Out of the Release Binary

Determinism costs nothing at runtime in production, and the build must cost nothing either. Three manifest rules keep the simulator, the explorer and `libc`/`mmap` out of a release.

**A library depends on the traits and the macros only.** moonpool-journal's manifest is the pattern:

```toml
[dependencies]
moonpool-core = { version = "0.8.0", default-features = false }
moonpool-buggify = "0.8.0"

[dev-dependencies]
moonpool-core = { version = "0.8.0", default-features = false, features = ["tokio-fs"] }
moonpool-sim = "0.8.0"
```

`default-features = false` on moonpool-core drops the tokio providers and the `select!` passthrough, so the library compiles with no tokio at all. The simulator enters only as a dev-dependency. The tests get `SimStorageProvider`; the release gets nothing.

**A binary enables the production providers it needs.** moonpool-core's features are granular: `tokio-task`, `tokio-time`, `tokio-net`, `tokio-fs`, `tokio-random`, `tokio-system`, with `tokio-providers` as the umbrella that gives `TokioProviders`. A consumer that only needs files enables `tokio-fs`. With every tokio feature off, moonpool-core is wasm-clean.

**The facade has a lean configuration.** The `moonpool` crate's default features are `["sim", "tokio"]`, the full framework. A production build opts out:

```toml
moonpool = { version = "0.8.0", default-features = false, features = ["tokio"] }
```

`tokio` enables core's `tokio-providers` and `select`, so `TokioProviders` and `moonpool::select!` are there. `sim` is off, so moonpool-sim and moonpool-explorer are absent from the dependency tree. `hyper`, `prometheus`, `rpc`, `rpc-derive`, `rpc-tls` and `rpc-jwt` are opt-in on top of either configuration.

## The select! Asymmetry

One thing to know when you read a lock file. `moonpool_core::select!` is tokio's macro verbatim with core's `select` feature, and tokio's expansion with a seeded offset with core's `deterministic-select` feature. moonpool-sim enables `deterministic-select`. Cargo unifies features across a build graph, so the moment moonpool-sim is in the graph, even as a dev-dependency of one crate, every crate in that graph gets the seeded macro, including your library compiled for tests. Both forms are tokio's expansion with identical grammar. The only change is where the start offset comes from, and outside a simulation that offset is entropy-seeded, as tokio's is. A release build without moonpool-sim keeps tokio's macro byte for byte.

## The Checklist

When you review a change against this rule, ask four questions:

1. Does the code reach tokio, `std::time`, `rand` or the filesystem without a provider? Move it behind `P: Providers`.
2. Does a test reach inside the component through a hook that production does not use? Replace the hook with a `hint!` or a `buggify_named!` site the simulator can drive.
3. Does the component have a `Sim*` twin? Delete the twin and run the real one under `SimProviders`.
4. Does the library's manifest depend on moonpool-sim outside `[dev-dependencies]`? Move it.

[Using Providers in Production](../part4-integration/05-production.md) shows the production side of the wiring, and [Migrating Existing Code to Providers](../part4-integration/06-migrating-existing-code.md) walks through an existing service.
