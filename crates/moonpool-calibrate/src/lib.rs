//! Measure the real machine, then write moonpool configuration for it.
//!
//! `moonpool-calibrate` answers one question: *what latency should the
//! simulation pretend to have?* It measures the host it runs on and prints Rust
//! source containing [`LatencyDistribution`] constants that feed moonpool's
//! existing storage and network knobs.
//!
//! ```text
//! real storage / network
//!         ↓  raw std I/O + std::time::Instant
//!     measurement
//!         ↓  HDR histogram
//!     p01 .. p99 envelope
//!         ↓  code generation
//!     LatencyDistribution::Uniform { start, end }
//!         ↓  existing seed-driven sampling
//!     deterministic simulated world
//! ```
//!
//! The crate ships the `moonpool-calibrate` binary. It is not published.
//!
//! ```text
//! moonpool-calibrate storage > measured_storage.rs
//! moonpool-calibrate network listen                       # on host B
//! moonpool-calibrate network measure host-b:7777 > measured_network.rs
//! ```
//!
//! # Quick example
//!
//! The library is the binary's building blocks. Record latencies, take the
//! envelope, and render a constant:
//!
//! ```rust
//! use std::time::Duration;
//!
//! use moonpool_calibrate::codegen::{Constant, GeneratedFile};
//! use moonpool_calibrate::stats::Latencies;
//!
//! let mut reads = Latencies::new("read");
//! for micros in 50..150 {
//!     reads.record(Duration::from_micros(micros));
//! }
//! let summary = reads.summary();
//! assert_eq!(summary.count, 100);
//!
//! let source = GeneratedFile::new("moonpool-calibrate storage")
//!     .constant(Constant::from_summary(
//!         "STORAGE_READ_LATENCY",
//!         "Measured latency of a 4096-byte read.",
//!         summary,
//!     ))
//!     .render();
//! assert!(source.contains("pub const STORAGE_READ_LATENCY: LatencyDistribution"));
//! ```
//!
//! # Features
//!
//! - [`storage`]: 4 KiB read, write and `sync_all` latency on one scratch
//!   file through raw `std::fs`.
//! - [`network`]: small-message TCP round-trip time through raw
//!   `std::net`, with an echo server ([`network::serve`]).
//! - [`stats`]: bounded HDR-histogram accounting ([`stats::Latencies`]) and
//!   the `p01..p99` envelope ([`stats::Summary`], [`stats::Bounds`]).
//! - [`codegen`]: Rust source with `LatencyDistribution` constants
//!   ([`codegen::GeneratedFile`], [`codegen::Constant`]).
//! - [`cli`]: the `clap` derive command surface. Help and errors go to
//!   stderr, because stdout carries only generated Rust.
//!
//! The generated constants assign into `StorageConfiguration` (`read_latency`,
//! `write_latency`, `sync_latency`) and `NetworkConfiguration`
//! (`write_latency`, `LinkLatencyConfig`). `NETWORK_RTT_LATENCY` is the
//! measured round trip; `NETWORK_LATENCY` is half of it, for moonpool's
//! one-way link knobs.
//!
//! # Feature flags
//!
//! None.
//!
//! # moonpool is bypassed while measuring
//!
//! This is the architectural rule of the crate. A measurement taken through
//! moonpool's providers would measure the simulator, not the machine, and the
//! result would be circular. So the measurement path uses only:
//!
//! - [`std::fs`] / [`std::io`] for storage,
//! - [`std::net::TcpListener`] / [`std::net::TcpStream`] for the network,
//! - [`std::time::Instant`] for timing.
//!
//! No provider trait, no simulated clock, no simulated randomness, no async
//! runtime. moonpool appears only in the *generated output* (and in
//! `tests/generated_api.rs`, which type-checks that output against the real
//! API).
//!
//! # Bounds, not extremes
//!
//! The generated range is the `p01..p99` envelope, never `min..max`: the tails
//! of a latency sample are dominated by scheduler preemption and page faults,
//! which would stretch the simulated world far past what the machine actually
//! does day to day. Full percentile diagnostics (`p01`, `p50`, `p95`, `p99`,
//! `max`, sample count) go to stderr.
//!
//! # What this is not
//!
//! Not `fio`, not `iperf`. There is no bandwidth measurement, no payload-size
//! sweep, no queue-depth exploration, no packet loss, no direct I/O, and no
//! distribution fitting — the storage methodology in particular is deliberately
//! simplistic and does not defeat the page cache. It exists to put plausible
//! numbers on moonpool's existing latency knobs, and nothing more.
//!
//! [`LatencyDistribution`]: https://docs.rs/moonpool-sim/latest/moonpool_sim/enum.LatencyDistribution.html

pub mod cli;
pub mod codegen;
pub mod network;
pub mod stats;
pub mod storage;
