//! Type-check the generated code against the real moonpool API.
//!
//! The constants below are a **verbatim copy** of real `moonpool-calibrate`
//! output (only the `use` lines are merged, since one file carries both runs).
//! If moonpool ever renames `LatencyDistribution`, its `Uniform` variant, or the
//! `start` / `end` fields, this file stops compiling — which is the signal to
//! update the generator and the golden test in `codegen.rs`.
//!
//! `cargo fmt --check` and `cargo clippy` also cover this file, so it doubles as
//! proof that the generator's output is rustfmt-clean and lint-clean in the
//! crate that consumes it.
//!
//! This is not a measurement path: moonpool is a dev-dependency only, used here
//! to validate generated *types*, never to take a timing.

use moonpool::LatencyDistribution;
use std::time::Duration;

/// Measured latency of a 4096-byte read.
///
/// p01 308ns, p50 556ns, p95 1.021µs, p99 1.616µs, max 77.695µs, n = 5000.
pub const STORAGE_READ_LATENCY: LatencyDistribution = LatencyDistribution::Uniform {
    start: Duration::from_nanos(308),
    end: Duration::from_nanos(1_616),
};

/// Measured latency of a 4096-byte write.
///
/// p01 391ns, p50 503ns, p95 853ns, p99 1.201µs, max 31.999µs, n = 5000.
pub const STORAGE_WRITE_LATENCY: LatencyDistribution = LatencyDistribution::Uniform {
    start: Duration::from_nanos(391),
    end: Duration::from_nanos(1_201),
};

/// Measured latency of `sync_all` with one dirty block outstanding.
///
/// p01 99.263µs, p50 129.855µs, p95 202.623µs, p99 261.503µs, max 2.092031ms, n = 5000.
pub const STORAGE_SYNC_LATENCY: LatencyDistribution = LatencyDistribution::Uniform {
    start: Duration::from_nanos(99_263),
    end: Duration::from_nanos(261_503),
};

/// Measured small-message TCP round-trip time.
///
/// p01 40.223µs, p50 44.031µs, p95 60.575µs, p99 94.079µs, max 284.671µs, n = 10000.
pub const NETWORK_RTT_LATENCY: LatencyDistribution = LatencyDistribution::Uniform {
    start: Duration::from_nanos(40_223),
    end: Duration::from_nanos(94_079),
};

/// One-way delay (round trip halved), for moonpool's one-way link knobs.
pub const NETWORK_LATENCY: LatencyDistribution = LatencyDistribution::Uniform {
    start: Duration::from_nanos(20_111),
    end: Duration::from_nanos(47_039),
};

/// The generated constants drop straight into the existing configuration types.
#[test]
fn generated_constants_plug_into_the_existing_configuration() {
    let storage = moonpool::storage::StorageConfiguration {
        read_latency: STORAGE_READ_LATENCY,
        write_latency: STORAGE_WRITE_LATENCY,
        sync_latency: STORAGE_SYNC_LATENCY,
        ..Default::default()
    };

    // moonpool's link knobs are documented as *one-way* delays, which is why the
    // generator emits the halved round trip alongside the measured RTT.
    let network = moonpool::network::NetworkConfiguration {
        write_latency: NETWORK_LATENCY,
        link_latency: Some(moonpool::LinkLatencyConfig {
            same_machine: NETWORK_LATENCY,
            same_zone: NETWORK_LATENCY,
            same_datacenter: NETWORK_LATENCY,
            cross_datacenter: NETWORK_LATENCY,
        }),
        ..Default::default()
    };

    for distribution in [
        &storage.read_latency,
        &storage.write_latency,
        &storage.sync_latency,
        &network.write_latency,
        &NETWORK_RTT_LATENCY,
    ] {
        let (start, end) = distribution
            .uniform_bounds()
            .expect("the generator only emits Uniform");
        assert!(start <= end, "generated bounds must never be inverted");
    }
}
