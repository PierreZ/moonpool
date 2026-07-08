# Simulation in the Browser

<!-- toc -->

The same simulation that grinds through thousands of seeds in CI also runs in a
browser tab, with no server and no install. Type a seed, watch your system take
traffic under a chaotic network, and share a link that reproduces the exact run
on anyone's machine. That makes a great demo. It is also a proof. If the engine
runs inside a sandbox with no operating system, then nothing in the simulation
secretly depends on one.

## Why a simulator compiles to wasm at all

In simulation the network and the disk are not devices. `SimNetwork` is an
in-memory state machine that schedules "deliver this packet at logical time T"
events on a queue. `SimStorage` is the same idea for reads and writes. Time is a
counter the engine advances. Randomness is a seeded ChaCha8 stream. The scheduler
is the [moonpool deterministic executor](../part2-foundations/11-executor.md),
plain library code. Add those up and there is no syscall anywhere on the path.

The production providers are the opposite. `TokioProviders` is real TCP, real
files, and OS randomness, every one of them a trip into the kernel. That single
difference is why the simulator crosses over to `wasm32-unknown-unknown` and the
production backend does not.

One piece used to be borrowed rather than owned. `SimProviders` once spawned
tasks through the real `TokioTaskProvider`, which meant shipping tokio's `rt`
feature to the browser. Not anymore: `SimTaskProvider` spawns onto the moonpool
deterministic executor, so the wasm build no longer needs core's `tokio-task`
feature at all. What remains of tokio in the graph is a thin, syscall-free
slice: `sync` for shutdown tokens and `macros` for the `select!` expansion.
The `net` and `fs` halves never cross over.

## What had to change

Getting there was four narrow fixes, not a rewrite.

- The fork-based explorer (`libc`, `fork`, `mmap`) moved behind a default-on
  `exploration` feature you switch off for wasm.
- Three wall-clock call sites the harness uses for reporting got a shim, because
  `Instant::now()` and `SystemTime::now()` compile on wasm and then **panic the
  moment you call them**.
- The `tokio` dependency narrowed to `sync` and `macros`. The deterministic
  executor owns the scheduling, so not even `rt` crosses over.
- `rand` dropped its default features so it never reaches for `getrandom` and the
  OS entropy that a browser has no answer for. The sim never needed it: its RNG
  is seeded ChaCha8.

## Does `block_on` park?

Compiling is necessary, not sufficient. A conventional runtime that runs out of
ready work tries to **park** the thread, and parking panics on
`wasm32-unknown-unknown`. So the real question was whether `block_on` ever parks
while driving a simulation.

It does not. The moonpool executor never parks at all: the simulation is
ready-driven, every wakeup fires inline as the orchestrator steps the event
queue, and there is no external reactor sitting on a socket or a timer to wake
the thread later. The executor always has the next step in front of it until
the run ends, and if nothing is runnable it treats that as a genuine deadlock
and panics with the seed instead of parking.

## Building a wasm-able crate

Putting a simulation in a browser is mostly a Cargo question, not a code
question. Your workload, your processes, and your invariants stay exactly as they
are. The one rule is to keep the heavy, non-portable dependencies out of the
build:

```toml
[dependencies]
moonpool-sim       = { version = "0.8", default-features = false }
moonpool-transport = { version = "0.8", default-features = false }
```

`default-features = false` drops the explorer and the production tokio providers
and leaves a simulator that targets wasm. Anything native, the same crate's
tests, a multi-seed chaos binary, a CI runner, depends on the same code with the
heavier features switched back on. Cargo resolves features per build, so
`cargo build -p your-wasm-crate` never drags exploration into the bundle. Write
the simulation once, run it in both places.

## A worked example

The repository ships one:
[`moonpool-wasm-demo`](https://github.com/PierreZ/moonpool/tree/main/moonpool-wasm-demo).
It runs a single seed of two nodes trading ping/pong RPCs over the **real
transport stack**, driven by the **simulated** network, and animates the result.
The client and server are ordinary `Process` and `Workload` code with no browser
awareness. `.enable_chaos([Chaos::Network(ChaosMode::Random)])` injects seeded latency and connection drops, so
some round trips come back slow and some never come back at all. A generic
recorder reads the same trace timeline your invariants read and turns it into the
picture.

It is running right here, compiled to wasm and served as a page asset. Press
**Run**, type a seed, or pick a preset. `256` is chaotic, `7` is a storm, `42` is
calm. The same seed always replays the same history.

<iframe
  src="../wasm-demo/index.html?embed=1&amp;seed=256"
  title="moonpool ping-pong wasm demo"
  loading="lazy"
  style="width:100%;height:640px;border:1px solid #30363d;border-radius:12px;background:#0d1117;color-scheme:dark">
</iframe>

You can also run it outside the book. The build is three commands and no bundler:

```bash
cargo build --release --target wasm32-unknown-unknown -p moonpool-wasm-demo --lib
wasm-bindgen --target web --out-dir web/pkg \
  target/wasm32-unknown-unknown/release/moonpool_wasm_demo.wasm
cd web && python3 -m http.server   # open the page, press Run
```

Run a seed natively with `cargo run -p moonpool-wasm-demo 42`, then run the same
seed in the browser. You get the **byte-identical** history. Same seed, same run,
on your laptop and in a stranger's tab. That is seed-driven reproducibility with
nowhere left to hide.

## What stays behind

Two things do not cross over, by design. The production providers cannot run in a
browser, because a tab has no raw TCP and no filesystem. And the explorer cannot
follow, because there is no `fork` in wasm. You lose multiverse forking, not
correctness: the in-process assertion table still tracks every `assert_always!`
and `assert_sometimes!`. The full platform matrix lives in
[Using Providers in Production](./05-production.md).

To keep the browser path from quietly rotting, a `portability` job in CI compiles
the simulator to `wasm32-unknown-unknown` on every change, runs the no-explorer
test suite, and fails the build if the lean production tree ever pulls the
simulator back in. The browser is not a one-time stunt. It is a gate.
