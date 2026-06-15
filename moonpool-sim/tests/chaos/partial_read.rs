//! Integration tests for partial read chaos injection.
//!
//! Tests verify that partial reads (mirroring FDB's `Sim2Conn` receiver):
//! - Deliver fewer bytes than available when BUGGIFY fires
//! - Leave the remainder buffered, preserving FIFO order (no data loss)
//! - Never return 0 bytes while data is buffered (which would stall `poll_read`)
//! - Are inert when BUGGIFY is disabled (baseline single-read behavior)
//!
//! `buggify!()` fires at ~25% per call (the macro's fixed firing probability),
//! so we cannot assert a cap on every individual read. Instead each seed asserts
//! the deterministic invariant — the payload reassembles correctly across short
//! reads with no zero-length read — and the fixed seed range collectively proves
//! that partial reads do occur (a fully-buffered payload split across >1 read).

use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_sim::{
    NetworkConfiguration, NetworkProvider, SimWorld, TcpListenerTrait, buggify_init, buggify_reset,
};

/// Drive one buggified exchange of `payload` with the given `partial_read_max_bytes`,
/// returning how many reads it took to drain the fully-buffered payload. Asserts
/// reassembly correctness and that no read returns 0 bytes while data is buffered.
async fn drain_with_partial_reads(seed: u64, max_bytes: usize, payload: &[u8]) -> usize {
    buggify_init(1.0, 1.0); // location always active; fires at the macro's 25%

    let mut config = NetworkConfiguration::fast_local();
    config.chaos.partial_read_max_bytes = max_bytes;

    let mut sim = SimWorld::new_with_network_config_and_seed(config, seed);
    let provider = sim.network_provider();

    let addr = "partial-read-server";
    let listener = provider.bind(addr).await.unwrap();
    let mut client = provider.connect(addr).await.unwrap();
    let (mut server, _) = listener.accept().await.unwrap();

    client.write_all(payload).await.unwrap();
    // Drain the network so every byte sits in the server's receive buffer.
    sim.run_until_empty();

    let mut received = Vec::new();
    let mut reads = 0;
    let mut buf = vec![0u8; payload.len()];
    while received.len() < payload.len() {
        let n = server.read(&mut buf).await.unwrap();
        assert!(
            n > 0,
            "seed {seed}: partial read returned 0 bytes while data was buffered (would stall poll_read)"
        );
        received.extend_from_slice(&buf[..n]);
        reads += 1;
    }

    assert_eq!(
        received, payload,
        "seed {seed}: data must reassemble in FIFO order"
    );
    reads
}

/// Across a fixed seed range, every seed reassembles the payload correctly and at
/// least one seed splits the fully-buffered payload across multiple reads.
#[test]
fn test_partial_reads_preserve_data() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let payload: Vec<u8> = (0u8..200).collect();
        let mut any_partial = false;
        for seed in 0..40u64 {
            let reads = drain_with_partial_reads(seed, 8, &payload).await;
            if reads > 1 {
                any_partial = true;
            }
        }
        assert!(
            any_partial,
            "expected at least one seed in 0..40 to deliver a fully-buffered payload across multiple reads"
        );
    });
}

/// Boundary case: `partial_read_max_bytes == 1` drives the at-least-one-byte rule
/// on its tightest edge — a fired read samples `1..2`, returning exactly one byte.
/// Guards against an off-by-one that would make the sample range empty and panic.
#[test]
fn test_partial_read_single_byte_boundary() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let payload: Vec<u8> = (0u8..32).collect();
        let mut any_partial = false;
        for seed in 0..40u64 {
            let reads = drain_with_partial_reads(seed, 1, &payload).await;
            if reads > 1 {
                any_partial = true;
            }
        }
        assert!(
            any_partial,
            "expected at least one seed in 0..40 to exercise the single-byte partial read path"
        );
    });
}

/// Without BUGGIFY, reads behave normally: a single read drains the whole buffer.
#[test]
fn test_partial_read_disabled_without_buggify() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        buggify_reset(); // BUGGIFY disabled -> no partial reads

        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        let addr = "no-partial-read-server";
        let listener = provider.bind(addr).await.unwrap();
        let mut client = provider.connect(addr).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();

        let payload: Vec<u8> = (0u8..100).collect();
        client.write_all(&payload).await.unwrap();
        sim.run_until_empty();

        let mut buf = vec![0u8; payload.len()];
        let n = server.read(&mut buf).await.unwrap();

        assert_eq!(
            n,
            payload.len(),
            "without BUGGIFY a single read returns all buffered bytes"
        );
        assert_eq!(&buf[..n], &payload[..], "data must be intact");

        println!("✅ Partial reads inert when BUGGIFY disabled");
    });
}
