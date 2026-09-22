//! The same RPC code as the `sim-rpc-foundations` campaign, on real TCP.
//!
//! A server runtime on its own OS thread (Tokio current-thread) and a
//! client runtime (Tokio multi-thread) share nothing but the bytes of a
//! service reference. The client calls the typed echo endpoint, then the
//! server destroys it and the client observes the terminal rejection.
//!
//! ```text
//! cargo run -p moonpool-rpc-sim --example tcp_echo
//! ```

use std::sync::mpsc;
use std::time::Duration;

use moonpool_core::TokioProviders;
use moonpool_rpc::{AccessClass, IncomingRequest, RpcConfig, RpcDriver, ServiceRef};
use moonpool_rpc_sim::foundations::messages::{Echo, Echoed, Probe};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (reference_tx, reference_rx) = mpsc::channel::<Vec<u8>>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();

    let server = std::thread::spawn(move || -> std::io::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let (driver, rpc) =
                RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", RpcConfig::default())
                    .await?;
            let driver = tokio::spawn(driver.run());
            let (service, mut stream) = rpc
                .register::<Echo>(AccessClass::Public)
                .map_err(std::io::Error::other)?;
            let _ = reference_tx.send(service.to_bytes());
            // Serve exactly one request, then destroy the endpoint.
            if let Some(IncomingRequest { request, reply }) = stream.recv().await {
                let _ = reply.send(&Echoed {
                    id: request.id,
                    text: request.text,
                });
            }
            drop(stream);
            let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
            driver.abort();
            Ok(())
        })
    });

    let bytes = reference_rx.recv()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let (driver, rpc) = RpcDriver::client_only(TokioProviders::new(), RpcConfig::default());
        let driver = tokio::spawn(driver.run());
        let echo = ServiceRef::<Echo>::from_bytes(&bytes)?.bind(&rpc);
        println!("calling {}", echo.target().endpoint());
        let reply = echo
            .try_get_reply_within(&Probe::new(1), Duration::from_secs(5))
            .await?;
        println!("reply {} {:?}", reply.id, reply.text);
        let rejected = echo
            .try_get_reply_within(&Probe::new(2), Duration::from_secs(5))
            .await;
        match rejected {
            Err(error) => println!(
                "after destruction: {} (execution: {:?})",
                error.reason(),
                error.execution()
            ),
            Ok(_) => return Err("a destroyed endpoint served a request".into()),
        }
        driver.abort();
        Ok::<(), Box<dyn std::error::Error>>(())
    })?;
    let _ = stop_tx.send(());
    server.join().map_err(|_| "server thread panicked")??;
    Ok(())
}
