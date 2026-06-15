# moonpool-wasm-demo

A KISS browser demo: run **one deterministic seed** of a ping-pong simulation —
over moonpool's **real transport stack**, driven by the **simulated network** —
as wasm, and watch the message exchange and the impact of chaos animate in a
browser tab.

Two nodes take part, both speaking the actual RPC stack:

- **Node B** is a server [`Process`] (the "ponger") that echoes every request.
- **Node A** is a client [`Workload`] (the "pinger") that sends 12 ping RPCs.

**The visualization sits on top of the workload, not inside it.** The workload is
a plain transport client — it just emits the standard observability events any
instrumented client would (`client_issued` / `client_acknowledged` /
`client_failed`, each carrying a `seq_id`) and contains *zero* visualization
code. A generic `TimelineRecorder`, registered as an ordinary moonpool
`Invariant`, observes those `tracing` events from the sim's timeline and
reconstructs the animated message timeline plus the injected network faults.
Because it keys off that standard event contract, the *same* recorder visualizes
any transport workload that emits it — swap in a consensus layer and you'd watch
its messages and chaos with no recorder changes.

`.enable_chaos([Chaos::Network(ChaosMode::Random)])` turns on seeded network chaos — variable latency, reordering,
connection drops — so a request comes back fast, slowly, or not at all. The
front-end animates each round trip as a ball flying A→B→A; a dropped request is a
ball lost at the net (red ✗). Nothing really waits: logical time is driven by the
sim event queue, which is exactly why the whole transport stack runs in a tab.

**The same seed always produces the same run — byte-for-byte, native and in the
browser.** That is the whole point of deterministic simulation.

## Run it natively (verify the workload first)

```sh
nix develop --command cargo run -p moonpool-wasm-demo 42
```

Prints the message timeline for seed 42 plus the JSON the browser receives. Try
seeds `7`, `256`, `1000` for heavy chaos; `42` is calm.

## Build & run in the browser

```sh
# 1. compile the cdylib to wasm (release = ~1 MB; drop --release for a fast dev build)
nix develop --command cargo build --release --target wasm32-unknown-unknown \
  -p moonpool-wasm-demo --lib

# 2. generate the JS/wasm bindings into web/pkg/
#    (wasm-bindgen-cli is pinned in flake.nix; the crate is pinned to match)
nix develop --command wasm-bindgen --target web --out-dir web/pkg \
  ../../target/wasm32-unknown-unknown/release/moonpool_wasm_demo.wasm

# 3. serve and open — no bundler, no npm
cd web && python3 -m http.server 8917
#   then open http://localhost:8917/
```

> Run step 2 from the `moonpool-wasm-demo/` directory (so `web/pkg` resolves),
> or pass absolute paths. `web/pkg/` is a build artifact and is gitignored.

### URL parameters

- `?seed=N` — load and run seed `N` (shareable, reproducible link).
- `?still=K` — render one frozen frame: the ball mid-flight at shot `K`, with
  every prior dropped request marked ✗. Deterministic; handy for screenshots.
- `?dump` — put the raw `runSeed` JSON in a hidden `<pre id="raw">` (used by the
  native↔browser reproducibility check).

## How it's wired (for the curious)

- `src/lib.rs`
  - `PongServer` (`Process`) + `PingClient` (`Workload`) — plain transport actors
    that emit standard `client_*` events; no visualization code.
  - `TimelineRecorder` (`Invariant`) — the generic, workload-agnostic layer that
    snapshots those events (and `sim_fault`s) off the trace timeline and rebuilds
    the `Shot` timeline.
  - `run_seed` / `run_seed_json` — register the recorder, run one seed, return
    the timeline. The wasm export `runSeed(seed)` lives behind
    `#[cfg(target_arch = "wasm32")]` and installs `console_error_panic_hook` so
    any panic surfaces in the console.
- `src/main.rs` — the native smoke runner.
- `web/index.html` — ~280 lines of vanilla JS: a canvas, two nodes, a flying
  ball, and the seeded chaos animation. No framework.

The crate depends on `moonpool-sim` and `moonpool-transport` with
`default-features = false` — the wasm-able configuration (no tokio net/fs, no
fork-based exploration). The single-threaded tokio runtime, the RPC stack, and
the seeded chaos all compile to and run on `wasm32-unknown-unknown`.

[`Process`]: https://docs.rs/moonpool-sim
[`Workload`]: https://docs.rs/moonpool-sim
