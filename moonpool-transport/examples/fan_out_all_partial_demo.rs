//! Fan-Out All Partial Example — per-peer health check.
//!
//! Demonstrates [`fan_out_all_partial`] against three peers. Unlike
//! [`fan_out_all`], this variant **never short-circuits on failure** — it
//! waits for every peer to respond (or fail) and returns a `Vec<Result>`
//! in input order, so the caller can correlate each outcome with its
//! sender by index.
//!
//! Useful when you want every peer's status, good or bad: diagnostics,
//! gossip-style state queries, per-peer health reporting.
//!
//! [`fan_out_all`]: moonpool_transport::fan_out_all
//!
//! # Run
//!
//! ```bash
//! cargo run --example fan_out_all_partial_demo
//! ```

use std::rc::Rc;

use moonpool_transport::{
    JsonCodec, NetTransportBuilder, NetworkAddress, Providers, RpcError, ServiceEndpoint,
    TaskProvider, TokioProviders, fan_out_all_partial, service,
};
use serde::{Deserialize, Serialize};

// ============================================================================
// Configuration
// ============================================================================

const PEER_ADDRS: [&str; 3] = ["127.0.0.1:4740", "127.0.0.1:4741", "127.0.0.1:4742"];
const CLIENT_ADDR: &str = "127.0.0.1:4743";

// ============================================================================
// Message Types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct HealthRequest;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct HealthResponse {
    peer_id: u32,
    healthy: bool,
    uptime_secs: u64,
}

// ============================================================================
// Interface Definition
// ============================================================================

#[service(id = 0x4845_414C)] // "HEAL"
trait HealthCheck {
    async fn check(&self, req: HealthRequest) -> Result<HealthResponse, RpcError>;
}

// ============================================================================
// Peer
// ============================================================================

async fn start_peer(
    providers: &TokioProviders,
    peer_id: u32,
    addr: NetworkAddress,
) -> Result<(), Box<dyn std::error::Error>> {
    let transport = Rc::new(
        NetTransportBuilder::new(providers.clone())
            .local_address(addr.clone())
            .build_listening()
            .await?,
    );
    let server = HealthCheckServer::init(&transport, JsonCodec);
    let transport_for_task = Rc::clone(&transport);

    providers
        .task()
        .spawn_task(&format!("health_peer_{}", peer_id), async move {
            while let Some((_req, reply)) = server
                .check
                .recv_with_transport::<_, HealthResponse>(&transport_for_task)
                .await
            {
                reply.send(HealthResponse {
                    peer_id,
                    healthy: true,
                    // Dummy uptime that varies per peer so the output is
                    // visibly distinct.
                    uptime_secs: 60 * (peer_id as u64 + 1),
                });
            }
        });

    println!("peer {} listening on {}", peer_id, addr);
    Ok(())
}

// ============================================================================
// Client
// ============================================================================

async fn run_client(providers: &TokioProviders) -> Result<(), Box<dyn std::error::Error>> {
    let client_addr = NetworkAddress::parse(CLIENT_ADDR)?;
    let transport = NetTransportBuilder::new(providers.clone())
        .local_address(client_addr)
        .build_listening()
        .await?;

    let peers: Vec<HealthCheckClient<JsonCodec>> = PEER_ADDRS
        .iter()
        .map(|addr_str| {
            let addr = NetworkAddress::parse(addr_str).expect("parse peer addr");
            HealthCheckClient::new(addr, JsonCodec)
        })
        .collect();

    let endpoints: Vec<ServiceEndpoint<HealthRequest, HealthResponse, JsonCodec>> =
        peers.iter().map(|c| c.check.clone()).collect();

    println!("\n=== Health-checking {} peers ===", endpoints.len());

    let results = fan_out_all_partial(&transport, &endpoints, HealthRequest).await?;

    println!("\nper-peer outcomes (input order):");
    for (i, result) in results.iter().enumerate() {
        match result {
            Ok(resp) => println!(
                "  peer {}: healthy={} uptime={}s",
                i, resp.healthy, resp.uptime_secs,
            ),
            Err(e) => println!("  peer {}: ERROR {:?}", i, e),
        }
    }

    Ok(())
}

// ============================================================================
// Main Entry Point
// ============================================================================

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("build tokio runtime");

    runtime.block_on(async {
        println!("=== fan_out_all_partial Example (health check) ===\n");

        let providers = TokioProviders::new();

        for (id, addr_str) in PEER_ADDRS.iter().enumerate() {
            let addr = NetworkAddress::parse(addr_str).expect("parse addr");
            if let Err(e) = start_peer(&providers, id as u32, addr).await {
                eprintln!("peer {} failed to start: {}", id, e);
                std::process::exit(1);
            }
        }

        if let Err(e) = run_client(&providers).await {
            eprintln!("client error: {}", e);
            std::process::exit(1);
        }
    });
}
