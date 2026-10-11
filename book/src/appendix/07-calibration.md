# Calibrating Against a Real Machine

<!-- toc -->

Every latency knob in the [Configuration Reference](./03-configuration.md) ships with a hand-picked default: storage reads at 50 to 200 µs, cross-datacenter links at 20 to 80 ms. Those numbers are plausible. They are not your numbers. A simulated disk ten times faster than production never shows the timeout cascade production is one bad afternoon away from.

`moonpool-calibrate` closes that gap. It measures the machine it runs on and prints Rust source with `LatencyDistribution` constants for the configuration types moonpool already has.

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

The calibrator decides plausible real-world **bounds**. Moonpool still decides the simulated **realization**: `sample_latency` draws from these distributions through the seeded simulation RNG, exactly as it does for the defaults. Determinism and replay do not change.

## Moonpool Is Bypassed While Measuring

This is the architectural rule of the tool. A measurement taken through moonpool's providers would measure the simulator, not the machine. So the measurement path touches no moonpool code:

| Concern | What the calibrator uses |
|---------|--------------------------|
| Storage | `std::fs::File`, `std::io::{Read, Write, Seek}`, `File::sync_all` |
| Network | `std::net::TcpListener`, `std::net::TcpStream` |
| Time | `std::time::Instant` |

No provider trait, no simulated clock, no simulated randomness, no async runtime. Moonpool appears only in the generated output, and in the crate's `tests/generated_api.rs`, which type-checks that output against the real API.

## Bounds Are p01 to p99, Not Min to Max

The generated range is the 1st-to-99th percentile envelope of the samples. The raw extremes come from scheduler preemption, page faults and one-off kernel work. Feeding them into a simulation stretches the simulated world far past what the machine does on an ordinary day. Full diagnostics (`p01`, `p50`, `p95`, `p99`, `max` and the sample count) go to stderr, so you can see the tail you chose not to encode.

Samples go into an HDR histogram (1 ns to 60 s, three significant figures), not a growing vector, so the sample count costs nothing in memory.

## The Command Surface

The crate is a workspace member and is not published. Run it with `cargo run -p moonpool-calibrate -- <command>`, or install the binary. The CLI is a `clap` derive: the commands are the `Command` and `NetworkCommand` enums, so the parser and the dispatch cannot drift apart.

One rule shapes the surface: **stdout belongs to generated Rust**. Help, version and parse errors go to stderr, so `moonpool-calibrate --help > out.rs` leaves `out.rs` empty instead of full of usage text.

## Storage

Run this on the machine whose disk the simulation should imitate:

1. Run `moonpool-calibrate storage > measured_storage.rs`.
2. Read the percentile table on stderr.
3. Add `measured_storage.rs` to your crate.

| Flag | Default | Meaning |
|------|---------|---------|
| `--file PATH` | `$TMPDIR/moonpool-calibrate.scratch` | Scratch file to measure against |
| `--samples N` | 1000 | Recorded samples per operation (at least 1) |
| `--warmup N` | 100 | Unrecorded warmup iterations per operation |

The method maps one-to-one onto moonpool's three storage latency knobs:

- **read**: seek to a block, `read_exact` 4096 bytes, fold the bytes into a checksum handed to `std::hint::black_box` so the work cannot be optimized away.
- **write**: seek to a block, `write_all` 4096 bytes. No sync.
- **sync**: dirty one block untimed, then time `sync_all` alone.

The 4 MiB scratch file (1024 blocks) is created and filled before timing starts, and removed by a drop guard, also on the error path. Each read and write targets a block chosen by a small deterministic PRNG, so the pattern is reproducible and not purely sequential.

A real run on a container filesystem, 5000 samples per operation:

```text
  operation         p01         p50         p95         p99         max  samples
  read            282ns       484ns     1.159µs     1.704µs    31.583µs     5000
  write           373ns       541ns       880ns     1.266µs    40.319µs     5000
  sync        109.695µs   140.927µs   202.623µs   253.311µs  3.248127ms     5000
```

The `max` column sits far from `p99`. That gap is why the generated bounds stop at p99.

## Network

The measuring side needs a peer.

1. On host B, run `moonpool-calibrate network listen`.
2. On host A, run `moonpool-calibrate network measure host-b:7777 > measured_network.rs`.
3. Stop the listener with ctrl-c.

| Flag | Default | Meaning |
|------|---------|---------|
| `--port P` (on `listen`) | 7777 | Port to bind on all interfaces |
| `--samples N` (on `measure`) | 1000 | Recorded round trips |
| `--warmup N` (on `measure`) | 100 | Unrecorded warmup round trips |

The protocol is the smallest thing that can be measured:

```text
client                          server
  |-- 8-byte sequence number ----->|
  |<-- the same 8 bytes -----------|
  RTT = elapsed
```

One connection carries every sample. `TCP_NODELAY` is on, so the measurement is not the delayed-ack timer. Each response is verified against the request that produced it. The listener serves one connection at a time, in accept order, because concurrency would only add scheduling noise.

## Plugging the Values In

The generated file is ordinary Rust:

```rust
// Generated by moonpool-calibrate. Do not edit by hand.
// ...
use moonpool::LatencyDistribution;
use std::time::Duration;

/// Measured latency of a 4096-byte read.
///
/// p01 282ns, p50 484ns, p95 1.159µs, p99 1.704µs, max 31.583µs, n = 5000.
pub const STORAGE_READ_LATENCY: LatencyDistribution = LatencyDistribution::Uniform {
    start: Duration::from_nanos(282),
    end: Duration::from_nanos(1_704),
};
```

The storage command emits `STORAGE_READ_LATENCY`, `STORAGE_WRITE_LATENCY` and `STORAGE_SYNC_LATENCY`. The network command emits `NETWORK_RTT_LATENCY` and `NETWORK_LATENCY`. Assign them into the existing configuration types:

```rust
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
        same_zone: measured_network::NETWORK_LATENCY,
        ..Default::default()
    }),
    ..Default::default()
};
```

In a `SimulationBuilder` campaign, `.link_latency(LinkLatencyConfig { .. })` takes the measured network value for the distance you measured. A hand-driven `SimWorld` takes both configurations through `set_network_config` and `set_storage_config`, and `set_process_storage_config(ip, config)` gives one process a measured disk. Measure each link class between the hosts that span it: a loopback run gives `same_machine`, two racks give `same_datacenter`, two regions give `cross_datacenter`.

### RTT Versus One-Way

`NETWORK_RTT_LATENCY` is the round trip as measured. `NETWORK_LATENCY` is that round trip halved. Moonpool's link knobs are documented as **one-way** delays, so `NETWORK_LATENCY` is the one that belongs in `LinkLatencyConfig` and `write_latency`. The RTT is kept for reference, because it is the number you can compare against `ping`.

## What This Tool Is Not

- **Not `fio`.** One scratch file, 4 KiB blocks, a deterministic block-pick pattern, and no attempt to defeat the page cache. No `O_DIRECT`, no queue-depth exploration, no payload sweep, no bandwidth or IOPS measurement. `iops` and `bandwidth` stay yours to set.
- **Not `iperf`.** Small-message TCP round-trip time only. Connection establishment is not timed, because moonpool has a separate `connect_latency` knob, and there is no bandwidth or packet-loss measurement.
- **Not a distribution fitter.** The output is always `Uniform`. A workload that needs the `Exponential` or `Bimodal` tail shapes picks them by hand from the percentile diagnostics.
- **Not a new configuration mechanism.** It emits values for the knobs that already exist, and those knobs are still sampled by the same seed-driven RNG.
