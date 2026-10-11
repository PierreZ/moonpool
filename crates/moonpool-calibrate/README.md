# moonpool-calibrate

Measure the real machine, then write moonpool configuration for it.

`moonpool-calibrate` answers one question: **what latency should the
simulation use?** It measures the host it runs on and prints Rust source with
`LatencyDistribution` constants for moonpool's storage and network knobs. The
crate is not published.

```text
real storage / network
        ↓  raw std I/O + std::time::Instant
    measurement
        ↓  HDR histogram
    p01 .. p99 envelope
        ↓  code generation
    LatencyDistribution::Uniform { start, end }
        ↓  seed-driven sampling, as for the hand-written defaults
    deterministic simulated world
```

## moonpool is bypassed while measuring

This is the architectural rule of the crate. A measurement taken through
moonpool's providers would measure the simulator, not the machine. The
measurement path therefore uses only:

| Concern | What is used |
|---|---|
| Storage | `std::fs::File`, `std::io::{Read, Write, Seek}`, `File::sync_all` |
| Network | `std::net::TcpListener`, `std::net::TcpStream` |
| Time | `std::time::Instant` |

No provider trait, no simulated clock, no simulated randomness, no async
runtime. moonpool appears only in the generated output, and in
`tests/generated_api.rs`, which type-checks that output against the real API.

## Usage

Diagnostics go to stderr and generated Rust goes to stdout, so you can
redirect a run straight into a source file. clap's help and errors also go to
stderr.

### Storage

Run this on the machine whose disk the simulation should imitate:

```bash
moonpool-calibrate storage > measured_storage.rs
```

| Flag | Default |
|---|---|
| `--file PATH` | `$TMPDIR/moonpool-calibrate.scratch` |
| `--samples N` | 1000 per operation |
| `--warmup N` | 100 per operation |

### Network

1. On host B, start the echo server:

   ```bash
   moonpool-calibrate network listen
   ```

2. On host A, measure the round trip to host B:

   ```bash
   moonpool-calibrate network measure host-b:7777 > measured_network.rs
   ```

`listen` takes `--port P` (default 7777). `measure` takes `--samples N`
(default 1000) and `--warmup N` (default 100).

## Using the generated values

The generated file is ordinary Rust that assigns into moonpool's existing
configuration types:

```rust
mod measured_storage;
mod measured_network;

use moonpool::{LinkLatencyConfig, NetworkConfiguration, StorageConfiguration};

let storage = StorageConfiguration {
    read_latency: measured_storage::STORAGE_READ_LATENCY,
    write_latency: measured_storage::STORAGE_WRITE_LATENCY,
    sync_latency: measured_storage::STORAGE_SYNC_LATENCY,
    ..Default::default()
};

let network = NetworkConfiguration {
    write_latency: measured_network::NETWORK_LATENCY,
    link_latency: Some(LinkLatencyConfig {
        same_machine: measured_network::NETWORK_LATENCY,
        ..Default::default()
    }),
    ..Default::default()
};
```

The calibrator finds plausible real-world **bounds**. moonpool still picks
each simulated value: it samples these distributions with the seeded
simulation RNG.

`NETWORK_RTT_LATENCY` is the measured round trip. `NETWORK_LATENCY` is half
of it. moonpool's link knobs are one-way delays, so plug in
`NETWORK_LATENCY`.

## Library

The binary is built from public modules: `storage`, `network`, `stats`
(`Latencies`, `Summary`, `Bounds`), `codegen` (`GeneratedFile`, `Constant`)
and `cli`. There are no cargo features.

## Deliberate limits

- **p01 .. p99, not min .. max.** Scheduler preemption and page faults
  dominate the extremes of a latency sample. Using them would stretch the
  simulated world far past what the machine does day to day.
- **Simple storage method.** One scratch file, 4 KiB blocks, a deterministic
  block-pick pattern, and no attempt to defeat the page cache. No
  `O_DIRECT`, no queue-depth exploration, no payload sweep.
- **Small-message TCP round trips only.** One connection, 8-byte ping/pong,
  `TCP_NODELAY` on. Connection setup is not timed (moonpool has a separate
  `connect_latency` knob). No bandwidth or packet-loss measurement.
- **Not `fio` and not `iperf`.** The only job is to put plausible numbers on
  moonpool's existing latency knobs.

## Documentation

- [Calibrating Against a Real Machine](https://pierrez.github.io/moonpool/appendix/07-calibration.html) in the Sim Book
