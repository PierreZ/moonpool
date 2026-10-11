# moonpool-wasm-demo

A small browser demo. It runs **one deterministic seed** of a raw TCP
ping-pong simulation as wasm, and animates the message exchange and the
impact of network chaos in a browser tab. The Sim Book embeds it. The crate
is not published.

Two nodes speak a tiny integrity-checked TCP protocol:

- **Node B** is a server `Process` (the ponger) that echoes every request.
- **Node A** is a client `Workload` (the pinger) that sends 12 ping frames.

`.enable_chaos([Chaos::Network(ChaosMode::Random)])` turns on seeded network
chaos: variable latency, reordering and connection drops. A request comes
back fast, slowly, or not at all. The page animates each round trip as a ball
that flies A→B→A. A dropped request is a ball lost at the net (red ✗).
Nothing really waits: the global scheduler advances logical time through
delayed network events, so the whole simulation runs in a tab.

**The same seed always produces the same run, byte for byte, natively and in
the browser.**

## The visualization sits on top of the workload

The workload is a plain network client. It emits the standard observability
events any instrumented client would (`client_issued`,
`client_acknowledged`, `client_failed`, `client_ambiguous`, each with a
`seq_id`). It contains no visualization code. A generic recorder, registered
as an ordinary moonpool `Invariant`, reads those `tracing` events from the
simulation timeline and rebuilds the message timeline and the injected
network faults. Any network workload that emits the same events gets the same
visualization.

## Run it natively

Verify the workload first:

```sh
cargo run -p moonpool-wasm-demo 42
```

This prints the message timeline for seed 42 and the JSON the browser
receives. Seed `42` is calm; try `7`, `256` or `1000` for heavy chaos.

## Build and run in the browser

1. Compile the library to wasm (release is about 1 MB):

   ```sh
   cargo build --release --target wasm32-unknown-unknown -p moonpool-wasm-demo --lib
   ```

2. Generate the JS bindings into `web/pkg/`. The `wasm-bindgen-cli` version
   must match the crate's pinned `wasm-bindgen` (0.2.129); the Nix flake
   pins it.

   ```sh
   wasm-bindgen --target web --out-dir crates/moonpool-wasm-demo/web/pkg \
     target/wasm32-unknown-unknown/release/moonpool_wasm_demo.wasm
   ```

3. Serve the page. There is no bundler and no npm:

   ```sh
   cd crates/moonpool-wasm-demo/web && python3 -m http.server 8917
   ```

4. Open <http://localhost:8917/>.

Run the commands from the repository root. `web/pkg/` is a build artifact and
is gitignored. `book/build-wasm-demo.sh` runs steps 1 and 2 and stages the
page for the book.

### URL parameters

| Parameter | Effect |
|---|---|
| `?seed=N` | Run seed `N`. The link is shareable and reproducible |
| `?still=K` | Render one frozen frame: the ball mid-flight at shot `K`, with every earlier dropped request marked ✗ |
| `?dump` | Put the raw `runSeed` JSON in a hidden `<pre id="raw">`, for the native and browser reproducibility check |

## How it is wired

| File | Role |
|---|---|
| `src/actors.rs` | `PongServer` (`Process`) and `PingClient` (`Workload`): plain TCP actors that emit `client_*` events |
| `src/protocol.rs` | The fixed-size, integrity-checked ping frame |
| `src/timeline.rs` | The workload-agnostic `Invariant` that turns trace events into the browser timeline |
| `src/model.rs` | The serializable `RunResult`, `Shot` and `Outcome` JSON model |
| `src/lib.rs` | `run_seed` / `run_seed_json`, and the wasm export `runSeed(seed)` with `console_error_panic_hook` |
| `src/main.rs` | The native smoke runner |
| `web/index.html` | Vanilla JS: a canvas, two nodes, a flying ball. No framework |

The crate depends on `moonpool-sim` with `default-features = false`: no tokio
providers and no fork-based exploration. The deterministic executor, the raw
TCP workload and the seeded chaos all compile to and run on
`wasm32-unknown-unknown`.

## Documentation

- [Simulation in the Browser](https://pierrez.github.io/moonpool/part4-integration/07-wasm.html) in the Sim Book
