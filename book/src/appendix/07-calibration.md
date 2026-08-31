# Calibrating Against a Real Machine

<!-- toc -->

Every latency knob in the [Configuration Reference](./03-configuration.md) ships
with a hand-picked default: storage reads at 50-200µs, cross-datacenter links at
20-80ms. Those numbers are plausible, but they are not your numbers. A simulation
whose disk is ten times faster than production will never surface the timeout
cascade production is one bad afternoon away from.

`moonpool-calibrate` closes that gap. It measures the machine it runs on and
prints Rust source containing `LatencyDistribution` constants we paste into the
configuration types moonpool already has.

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

The calibrator determines plausible real-world **bounds**. moonpool still
determines the simulated **realization**: `sample_latency` draws from these
distributions through the seeded simulation RNG, exactly as it does for the
hand-written defaults. Nothing about determinism or replay changes.

## moonpool is deliberately bypassed during measurement

This is the architectural rule of the tool, and it is worth stating plainly. A
measurement taken through moonpool's providers would measure the **simulator**,
not the machine, and the result would be circular. So the measurement path
touches no moonpool code at all.

| Concern | What the calibrator uses |
|---------|--------------------------|
| Storage | `std::fs::File`, `std::io::{Read, Write, Seek}`, `File::sync_all` |
| Network | `std::net::TcpListener`, `std::net::TcpStream` |
| Time    | `std::time::Instant` |

No `StorageProvider`, no `NetworkProvider`, no `TimeProvider`, no simulated
clock, no simulated randomness, no async runtime. moonpool appears only in the
generated output, and in the crate's `tests/generated_api.rs`, which type-checks
that output against the real API.

## Bounds are p01 .. p99, not min .. max

The generated range is the 1st-to-99th percentile envelope of the samples. Raw
extremes are dominated by scheduler preemption, page faults, and one-off kernel
work. Feeding them into a simulation stretches the simulated world far past what
the machine does on an ordinary day. Full diagnostics (`p01`, `p50`, `p95`,
`p99`, `max`, and the sample count) go to stderr, so we can see the tail we chose
not to encode.

Samples accumulate into an HDR histogram (1ns to 60s, three significant figures)
rather than a growing vector, so sample count costs nothing in memory.

## The command surface

The CLI is a `clap` derive: the commands are enum variants (`Command`,
`NetworkCommand`), so the parser and the dispatch `match` cannot drift apart.
One deviation from clap's defaults is deliberate. Help, version, and parse errors
are rendered onto **stderr** rather than stdout, because stdout belongs to
generated Rust. `moonpool-calibrate --help > out.rs` leaves `out.rs` empty rather
than full of usage text.

## Storage

Run this on the machine whose disk the simulation should imitate:

```bash
moonpool-calibrate storage > measured_storage.rs
```

Diagnostics land on stderr, generated Rust on stdout, so the redirect above
produces a compilable file.

| Flag | Default | Meaning |
|------|---------|---------|
| `--file PATH` | `$TMPDIR/moonpool-calibrate.scratch` | Scratch file to measure against |
| `--samples N` | 1000 | Recorded samples per operation |
| `--warmup N` | 100 | Unrecorded warmup iterations per operation |

The methodology maps one-to-one onto moonpool's three storage latency knobs:

- **read**: seek to a block, `read_exact` 4096 bytes, fold the bytes into a
  checksum handed to `std::hint::black_box` so the work cannot be optimised away.
- **write**: seek to a block, `write_all` 4096 bytes. `sync` is not included.
- **sync**: dirty one block untimed, then time `sync_all` on its own.

The 4 MiB scratch file is created and filled before timing starts, and removed by
a drop guard even when the run fails.

A real run on a container filesystem, 5000 samples per operation:

```text
  operation         p01         p50         p95         p99         max  samples
  read            282ns       484ns     1.159µs     1.704µs    31.583µs     5000
  write           373ns       541ns       880ns     1.266µs    40.319µs     5000
  sync        109.695µs   140.927µs   202.623µs   253.311µs  3.248127ms     5000
```

Note how far the `max` column sits from `p99`. That gap is exactly why the
generated bounds stop at p99.

## Network

The measuring side needs a peer. On host B:

```bash
moonpool-calibrate network listen
```

On host A:

```bash
moonpool-calibrate network measure host-b:7777 > measured_network.rs
```

| Flag | Default | Meaning |
|------|---------|---------|
| `--port P` (on `listen`) | 7777 | Port to bind on all interfaces |
| `--samples N` | 1000 | Recorded round trips |
| `--warmup N` | 100 | Unrecorded warmup round trips |

The protocol is the smallest thing that can be measured:

```text
client                          server
  |-- 8-byte sequence number ----->|
  |<-- the same 8 bytes -----------|
  RTT = elapsed
```

One connection carries every sample, `TCP_NODELAY` is on so the measurement is
not the delayed-ack timer, and each response is verified against the request that
produced it.

## Plugging the values in

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

Assign the constants into the existing configuration types:

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

### RTT versus one-way

The network command emits two constants. `NETWORK_RTT_LATENCY` is the round trip
as measured. `NETWORK_LATENCY` is that round trip halved. moonpool's link knobs
are documented as **one-way** delays, so `NETWORK_LATENCY` is the one that
belongs in `LinkLatencyConfig` and `write_latency`. The RTT is kept for
reference, because it is the number we can compare against `ping`.

## What this tool is not

- Not `fio`. Storage calibration is intentionally simplistic: one scratch file,
  4 KiB blocks, a deterministic block-pick pattern, and no attempt to defeat the
  page cache. There is no `O_DIRECT`, no queue-depth exploration, no payload
  sweep, no bandwidth or IOPS measurement.
- Not `iperf`. Network calibration measures small-message TCP round-trip time
  only. Connection establishment is not timed, because moonpool has a separate
  `connect_latency` knob, and there is no bandwidth or packet-loss measurement.
- Not a distribution fitter. The output is always `Uniform`. A workload that
  needs the `Exponential` or `Bimodal` tail shapes picks them by hand from the
  percentile diagnostics. The calibrator will not guess a shape.
- Not a new configuration mechanism. It emits values for the knobs that already
  exist, and those knobs are still sampled by the same seed-driven RNG.
