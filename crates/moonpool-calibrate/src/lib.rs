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
