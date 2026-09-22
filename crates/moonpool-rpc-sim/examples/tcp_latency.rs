//! Baseline round-trip latency of one typed call over localhost TCP.
//!
//! A calibration point for later packages, not a benchmark suite: one
//! client, one server, sequential calls on one session, both runtimes on a
//! multi-thread Tokio runtime.
//!
//! ```text
//! cargo run --release -p moonpool-rpc-sim --example tcp_latency [calls]
//! ```

use std::time::Instant;

use moonpool_core::TokioProviders;
use moonpool_rpc::{AccessClass, IncomingRequest, RpcConfig, RpcDriver};
use moonpool_rpc_sim::foundations::messages::{Echo, Echoed, Probe};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let calls: u32 = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(20_000);
    let (server_driver, server) =
        RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", RpcConfig::default()).await?;
    let server_driver = tokio::spawn(server_driver.run());
    let (service, mut stream) = server.register::<Echo>(AccessClass::Public)?;
    let handler = tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            let _ = reply.send(&Echoed {
                id: request.id,
                text: request.text,
            });
        }
    });
    let (client_driver, client) =
        RpcDriver::client_only(TokioProviders::new(), RpcConfig::default())?;
    let client_driver = tokio::spawn(client_driver.run());
    let echo = service.bind(&client);

    // Warm the session up (connect, handshake) outside the measurement.
    echo.try_get_reply(&Probe::new(0)).await?;
    let mut samples = Vec::with_capacity(usize::try_from(calls)?);
    let started = Instant::now();
    for id in 1..=calls {
        let call = Instant::now();
        echo.try_get_reply(&Probe::new(u64::from(id))).await?;
        samples.push(call.elapsed());
    }
    let total = started.elapsed();
    samples.sort();
    let percentile = |per_mille: usize| samples[(samples.len() - 1) * per_mille / 1000];
    println!(
        "{calls} sequential calls in {total:?}: {:.0} calls/s, p50 {:?}, p99 {:?}, max {:?}",
        f64::from(calls) / total.as_secs_f64(),
        percentile(500),
        percentile(990),
        percentile(1000)
    );
    handler.abort();
    server_driver.abort();
    client_driver.abort();
    Ok(())
}
