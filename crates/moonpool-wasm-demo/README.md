# moonpool-wasm-demo

The run engine of the moonpool website. It runs **one deterministic seed** of
a small moonpool simulation, natively or as wasm in a browser tab, and returns
what the run recorded as JSON. Every live run of the site is this crate. The
crate is not published.

## The small system

A replicated register (`src/register.rs`):

- three server `Process`es keep each `(version, value)` record in a log
  file, one 512-byte sector per version, and `sync_all` it before they answer.
  At boot a server reads the log and keeps the newest record that passes its
  check word;
- one client `Workload` writes versions 1 to 8 to all servers and calls a
  write committed when a quorum answered, then reads from a quorum;
- `RegisterInvariant` checks across the processes, from the trace events
  alone, that a read returns the last committed write.

The code has `assert_sometimes!` on its rare paths, a `hint!` between the
write and its sync, a `hint!` between the durable write and the answer, and a
`buggify!` that drops an answer. A planted
bug, off by default, makes the client read from one server in place of a
quorum: with the default chaos (network faults and reboots), about 12% of
seeds break the invariant. With the bug off, no seed of a 2,000-seed run
fails.

## The engine

`run(&RunConfig)` (and `run_json`, JSON in and JSON out) adds one seed and a
recorder to the simulation. The recorder is an ordinary `Invariant` that reads
the trace events through `TraceQuery`, with the injected faults under
`SIM_FAULT_EVENT_NAME`. The scenario holds no drawing code.

The config:

```json
{ "scenario": "register", "seed": 256, "servers": 3, "bug": false,
  "chaos": { "network": true, "storage": false, "reboots": true },
  "network_faults": null, "storage_faults": null, "wipes": false,
  "swarm": false, "recipe": null }
```

Each field is optional. `network_faults` and `storage_faults` keep only the
named fault families (`NETWORK_FAULTS` and `STORAGE_FAULTS` in
`src/engine.rs`); `null` keeps them all. `wipes` lets attrition wipe the disk
of one server, which comes back within the run on an empty log. `swarm`
runs the network and storage chaos in `ChaosMode::Swarm`, and the run then
names the families the seed drew in `families`. `recipe` replays one timeline
of an exploration (`replay_timeline`) in place of the plain seed. The run (`Run` in `src/engine.rs`, versioned by
`MODEL`) holds the processes, the trace events, the faults, the messages, the
assertion counts, the summary of the report with the first event that broke
the invariant, and a fingerprint: a hash of the events and the count of random
draws.

## Coverage

`coverage(&CoverageConfig)` (and `coverage_json`, exported to wasm as
`coverage`) runs seeds one after another from a run config and records which
of the register's checks (`register::CHECKS`, its `assert_sometimes!` and
`assert_reachable!` sites) each seed hit first. It stops when every check
fired and `plateau` seeds in a row hit nothing new, as `UntilCoverageStable`
does, or at `max_seeds`, with the checks still missing. With the `bit_flip`
family off, "a server drops a damaged frame" can never fire.

## Run it

```sh
# One config natively, as JSON
cargo run -p moonpool-wasm-demo --bin site-run -- '{"seed": 42}'

# The same configs natively and in wasm under node, compared byte for byte
scripts/check-site-wasm.sh 1 7 42 256

# The system as an example simulation, each seed run twice
cargo xtask sim run register
```

In a browser: `website/site/scripts/build-wasm-demo.sh` builds the wasm and
the JS glue into `website/site/public/wasm-demo/pkg/`. The site's
`public/engine-worker.js` loads it in a Web Worker and calls
`run(configJson)`. The `wasm-bindgen` crate version must match the CLI of the
Nix flake exactly (see `AGENTS.md`).

The crate depends on `moonpool-sim` with `default-features = false`: no tokio
providers and no fork-based exploration, which is the configuration that
builds for `wasm32-unknown-unknown`.

## Documentation

- [Simulation in the Browser](https://pierrez.github.io/moonpool/part4-integration/07-wasm.html) in the Sim Book
- [One thread, one clock, one seed](https://pierrez.github.io/moonpool/techniques/determinism/) on the website
- [Multiverse exploration](https://pierrez.github.io/moonpool/techniques/exploration/): its tree is recorded natively by `tests/exploration_tree.rs`
