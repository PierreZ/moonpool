//! Fan-Out Race Example — hedged read, first success wins.
//!
//! Demonstrates [`fan_out_race`] against three equivalent read replicas.
//! The client sends the request to all three in parallel and returns the
//! **first** successful reply. Pending in-flight requests for the slower
//! replicas are dropped (cancelled) when the winner is chosen.
//!
//! This is the simplest form of tail-latency hedging: useful when every
//! replica holds the same data and the client just wants the fastest
//! response.
//!
//! Note: under real TCP loopback within a single process the "winner" is
//! whichever replica's future happens to poll first after the queues fill
//! — the API shape is what matters, not which replica wins.
//!
//! # Run
//!
//! ```bash
//! cargo run --example fan_out_race_demo
//! ```

use std::rc::Rc;

use moonpool_transport::{
    JsonCodec, NetTransportBuilder, NetworkAddress, Providers, RpcError, ServiceEndpoint,
    TaskProvider, TokioProviders, fan_out_race, service,
};
use serde::{Deserialize, Serialize};

// ============================================================================
// Configuration
// ============================================================================

const REPLICA_ADDRS: [&str; 3] = ["127.0.0.1:4730", "127.0.0.1:4731", "127.0.0.1:4732"];
const CLIENT_ADDR: &str = "127.0.0.1:4733";

// ============================================================================
// Message Types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ReadRequest {
    key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ReadResponse {
    replica_id: u32,
    value: String,
}

// ============================================================================
// Interface Definition
// ============================================================================

#[service(id = 0x4852_4541)] // "HREA" (hedged read)
trait HedgedRead {
    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, RpcError>;
}

// ============================================================================
// Replica
// ============================================================================

async fn start_replica(
    providers: &TokioProviders,
    replica_id: u32,
    addr: NetworkAddress,
) -> Result<(), Box<dyn std::error::Error>> {
    let transport = Rc::new(
        NetTransportBuilder::new(providers.clone())
            .local_address(addr.clone())
            .build_listening()
            .await?,
    );
    let server = HedgedReadServer::init(&transport, JsonCodec);
    let transport_for_task = Rc::clone(&transport);

    providers
        .task()
        .spawn_task(&format!("hedged_replica_{}", replica_id), async move {
            while let Some((req, reply)) = server
                .read
                .recv_with_transport::<_, ReadResponse>(&transport_for_task)
                .await
            {
                reply.send(ReadResponse {
                    replica_id,
                    value: format!("replica-{}:{}", replica_id, req.key),
                });
            }
        });

    println!("replica {} listening on {}", replica_id, addr);
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

    let replicas: Vec<HedgedReadClient<JsonCodec>> = REPLICA_ADDRS
        .iter()
        .map(|addr_str| {
            let addr = NetworkAddress::parse(addr_str).expect("parse replica addr");
            HedgedReadClient::new(addr, JsonCodec)
        })
        .collect();

    let endpoints: Vec<ServiceEndpoint<ReadRequest, ReadResponse, JsonCodec>> =
        replicas.iter().map(|c| c.read.clone()).collect();

    println!("\n=== Hedged read against {} replicas ===", endpoints.len());

    let winner = fan_out_race(
        &transport,
        &endpoints,
        ReadRequest {
            key: "hello".into(),
        },
    )
    .await?;

    println!(
        "first reply came from replica {} with value {:?}",
        winner.replica_id, winner.value,
    );

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
        println!("=== fan_out_race Example (hedged read) ===\n");

        let providers = TokioProviders::new();

        for (id, addr_str) in REPLICA_ADDRS.iter().enumerate() {
            let addr = NetworkAddress::parse(addr_str).expect("parse addr");
            if let Err(e) = start_replica(&providers, id as u32, addr).await {
                eprintln!("replica {} failed to start: {}", id, e);
                std::process::exit(1);
            }
        }

        if let Err(e) = run_client(&providers).await {
            eprintln!("client error: {}", e);
            std::process::exit(1);
        }
    });
}
