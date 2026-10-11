# moonpool-buggify

Fault-injection points for shipped code: `buggify!`, `hint!` and
`cpu_charge`, with zero dependencies.

Shipped code marks the places where a simulator may make life hard. In
production every mark is inert. In a `moonpool-sim` run the simulator
decides, from the seed, what happens there. The code never runs a `Sim*`
variant of itself. The crate has no dependencies and compiles on
`wasm32-unknown-unknown`, macOS and Linux, so a production crate or a
sans-I/O library can depend on it without the simulator.

| Mark | The code says | The simulator may |
|------|---------------|-------------------|
| `buggify!` and friends | "a rare but legal branch is here" | force the branch |
| `hint!` | "a crash right here would be interesting" | reboot the calling process |
| `cpu_charge` | "I just did this much CPU work" | move the process clock forward |

## Buggify

Buggify comes from FoundationDB. Each location is **activated** once per
run, on its first encounter (`moonpool-sim` uses probability 0.5). An active
location then **fires** on some of its calls. A bug that needs a rare branch
becomes likely instead of lucky.

```rust
use moonpool_buggify::{buggify, buggify_range, buggify_with_prob};

fn flush(buffer: &mut Vec<u8>) {
    if buggify!() {
        // An active location fires on 25% of calls: take the slow path.
    }
    if buggify_with_prob!(0.01) {
        // A site rate of its own.
    }
    // Stretch a value: Some only when the location is active and fires.
    let _batch = buggify_range!(0.1, 1..4).unwrap_or(64);
    buffer.clear();
}
```

| Macro | Result | Silent in the recovery tail |
|-------|--------|-----------------------------|
| `buggify!` | `bool`, 25% per call while active | no |
| `buggify_with_prob!` | `bool`, at a site rate | no |
| `buggify_fault_with_prob!` | `bool`, for a site that makes an operation fail | yes |
| `buggify_named!` | `bool`, keyed by a label a harness forces with `set_activation` | yes |
| `buggify_pick!` | `Option<usize>`: an index in `0..n` | no |
| `buggify_range!` | `Option<u64>`: a value in a range | no |

`buggify_named!` with `set_activation` builds per-seed scenarios: a harness
switches several labelled locations on or off together, instead of hoping
for the product of independent activation coins. `buggify_knob!`, which
moves a tunable to an extreme, lives in `moonpool-sim`.

When the simulation's chaos window closes, the runtime calls
`buggify_enter_recovery`. The disruptive sites then stay silent, so the
system gets a quiet tail to recover in.

## Hints

```rust
async fn commit() {
    // ... the batch is synced ...
    moonpool_buggify::hint!("batch durable, not sent").await;
    // ... now send it to the peers ...
}
```

`hint!` names a moment, never a fault. In a simulation the point is a
disruptive buggify location (5% per call while active, or a site rate:
`hint!("label", 0.2)`). When it fires, the simulator may reboot the calling
process there, under the seed's attrition regime, its `max_dead` budget and
an optional harness `HintVeto`. A killed caller's future never resolves.
Outside a simulation the future resolves at once. `moonpool-journal` puts
three hints inside each commit.

## CPU charges

`cpu_charge(cost)` tells a simulator with a CPU model that the caller just
did `cost` of work. The process clock moves forward by that cost times the
machine's slowness, so gray-failure scenarios reach the code. Put charges
only on hot paths a profile shows.

## For simulation runtimes

`set_random_source` installs the seeded source, `buggify_init` starts a run,
`hint::set_sink` and `cpu::set_sink` install the simulator's sides,
`buggify_enter_recovery` closes the chaos window and `buggify_reset` ends
the run. `is_simulated` says whether a simulation is running. State is
thread-local, and every path into the crate (for example
`moonpool_sim::buggify!`) shares it.

## Documentation

- [Buggify](https://pierrez.github.io/moonpool/part3-building/08-buggify.html) and
  [hint!](https://pierrez.github.io/moonpool/part3-building/29-hints.html) in the Sim Book
- [API documentation](https://docs.rs/moonpool-buggify)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
