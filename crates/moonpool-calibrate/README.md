# moonpool-calibrate

Measure the real machine, then write moonpool configuration for it.

`moonpool-calibrate` answers one question: *what latency should the simulation
pretend to have?* It measures the host it runs on and prints Rust source
containing `LatencyDistribution` constants that feed moonpool's existing storage
and network knobs.

```text
real storage / network
        ↓  raw std I/O + std::time::Instant
    measurement
        ↓  HDR histogram
    p01 .. p99 envelope
        ↓  code generation
    LatencyDistribution::Uniform { start, end }
        ↓  existing seed-driven sampling
    deterministic simulated world
```

## moonpool is bypassed while measuring

This is the architectural rule of the crate. A measurement taken through
moonpool's providers would measure the simulator, not the machine, and the result
would be circular. The measurement path therefore uses only:

| Concern | What is used |
|---------|--------------|
| storage | `std::fs::File`, `std::io::{Read, Write, Seek}`, `File::sync_all` |
| network | `std::net::TcpListener`, `std::net::TcpStream` |
| time    | `std::time::Instant` |

No provider trait, no simulated clock, no simulated randomness, no async runtime.
moonpool appears only in the *generated output* — and in
`tests/generated_api.rs`, which type-checks that output against the real API.

## Usage

Diagnostics go to stderr and generated Rust goes to stdout, so a run can be
redirected straight into a source file.

### Storage

Run this on the machine whose disk the simulation should imitate:

```bash
moonpool-calibrate storage > measured_storage.rs
```

Flags: `--file PATH` (scratch file, default `$TMPDIR/moonpool-calibrate.scratch`),
`--samples N` (default 1000 per operation), `--warmup N` (default 100).

### Network

On host B:

```bash
moonpool-calibrate network listen
```

On host A:

```bash
moonpool-calibrate network measure host-b:7777 > measured_network.rs
```

Flags: `--port P` on `listen` (default 7777), `--samples N` / `--warmup N` on
`measure`.

## Using the generated values

The generated file is ordinary Rust that assigns into the configuration types
moonpool already has:

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

The calibrator determines plausible real-world *bounds*. moonpool still
determines the simulated *realization*: `sample_latency` draws from these
distributions through the seeded simulation RNG, exactly as it does for the
hand-written defaults.

Note that `NETWORK_RTT_LATENCY` is the measured round trip while
`NETWORK_LATENCY` is that round trip halved. moonpool's link knobs are
documented as one-way delays, so `NETWORK_LATENCY` is the one to plug in.

## Deliberate limits

- **p01 .. p99, not min .. max.** The extremes of a latency sample are dominated
  by scheduler preemption and page faults; using them would stretch the simulated
  world far past what the machine actually does day to day.
- **Storage calibration is intentionally simplistic.** One scratch file, 4 KiB
  blocks, a deterministic block-pick pattern, and no attempt to defeat the page
  cache. There is no `O_DIRECT`, no queue-depth exploration, no payload sweep.
- **Network calibration measures small-message TCP round-trip time only.** One
  connection, 8-byte ping/pong, `TCP_NODELAY` on. Connection establishment is not
  timed (moonpool has a separate `connect_latency` knob), and there is no
  bandwidth or packet-loss measurement.
- **This is not `fio` and not `iperf`.** It is a calibration utility whose only
  job is to put plausible numbers on moonpool's existing latency knobs.
