//! Well-Known Endpoint Example: Deterministic token addressing.
//!
//! This example demonstrates moonpool's well-known endpoint pattern where
//! both server and client derive tokens from a compile-time constant —
//! no service discovery needed.
//!
//! Run as two separate processes:
//!
//! ```bash
//! # Terminal 1 - Start the server
//! cargo run --example well_known_endpoint -- server
//!
//! # Terminal 2 - Run the client
//! cargo run --example well_known_endpoint -- client
//! ```
//!
//! # Architecture
//!
//! - `#[service]` macro generates Server and Client types
//! - `transport.serve_well_known::<PingPong>(token_id, codec)` registers at deterministic token
//! - `NetTransport::client_well_known::<PingPong>(addr, token_id, codec)` connects without discovery

use std::env;
use std::time::Duration;

use moonpool_transport::{
    JsonCodec, NetTransportBuilder, NetworkAddress, Providers, RpcError, TimeProvider,
    TokioProviders, service,
};
use serde::{Deserialize, Serialize};

// ============================================================================
// Configuration
// ============================================================================

const SERVER_ADDR: &str = "127.0.0.1:4500";
const CLIENT_ADDR: &str = "127.0.0.1:4501";

/// Well-known token for the PingPong service.
/// Starts at FirstAvailable (3) + 1 to avoid collisions with system tokens.
const WLTOKEN_PING_PONG: u32 = 4;

// ============================================================================
// Message Types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PingRequest {
    seq: u32,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PingResponse {
    seq: u32,
    echo: String,
}

// ============================================================================
// Interface Definition
// ============================================================================

#[service]
trait PingPong {
    async fn ping(&self, req: PingRequest) -> Result<PingResponse, RpcError>;
}

// ============================================================================
// Server
// ============================================================================

async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Well-Known Endpoint Server ===\n");

    let providers = TokioProviders::new();
    let local_addr = NetworkAddress::parse(SERVER_ADDR)?;

    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Server listening on {}\n", SERVER_ADDR);

    let ping_server = PingPongServer::well_known(&transport, WLTOKEN_PING_PONG, JsonCodec);

    println!(
        "Registered {} method(s) at well-known token {}\n",
        PingPongServer::METHOD_COUNT,
        WLTOKEN_PING_PONG
    );
    println!("Waiting for ping requests...\n");

    loop {
        if let Some((request, reply)) = ping_server.ping.recv().await {
            println!("Received ping seq={}: {:?}", request.seq, request.message);

            let response = PingResponse {
                seq: request.seq,
                echo: format!("pong: {}", request.message),
            };

            reply.send(response.clone());
            println!("Sent pong seq={}: {:?}\n", response.seq, response.echo);
        } else {
            println!("Request stream closed, shutting down.");
            break;
        }
    }

    Ok(())
}

// ============================================================================
// Client
// ============================================================================

async fn run_client() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Well-Known Endpoint Client ===\n");

    let providers = TokioProviders::new();
    let time = providers.time().clone();

    let local_addr = NetworkAddress::parse(CLIENT_ADDR)?;
    let server_addr = NetworkAddress::parse(SERVER_ADDR)?;

    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Connecting to server at {}\n", SERVER_ADDR);

    // Client constructed from well-known token — no discovery needed.
    let ping_client =
        PingPongClient::well_known(server_addr, WLTOKEN_PING_PONG, JsonCodec, &transport);

    println!(
        "Using well-known token {} with {} method(s)\n",
        WLTOKEN_PING_PONG,
        PingPongClient::METHOD_COUNT
    );

    let num_pings = 5;
    let mut success_count = 0;

    for seq in 0..num_pings {
        let request = PingRequest {
            seq,
            message: format!("hello from client (seq={})", seq),
        };

        println!("Sending ping seq={}: {:?}", seq, request.message);

        match time
            .timeout(Duration::from_secs(5), ping_client.ping.get_reply(request))
            .await
        {
            Ok(Ok(response)) => {
                println!("Received pong seq={}: {:?}\n", response.seq, response.echo);
                success_count += 1;
            }
            Ok(Err(e)) => {
                println!("RPC error: {:?}\n", e);
            }
            Err(e) => {
                println!("Timeout or shutdown: {:?}\n", e);
            }
        }

        let _ = time.sleep(Duration::from_millis(100)).await;
    }

    println!("=== Results ===");
    println!(
        "{}/{} pings completed successfully!",
        success_count, num_pings
    );

    Ok(())
}

// ============================================================================
// Main Entry Point
// ============================================================================

fn main() {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to create Tokio LocalRuntime");

    match mode {
        "server" => {
            runtime.block_on(async {
                if let Err(e) = run_server().await {
                    eprintln!("Server error: {}", e);
                    std::process::exit(1);
                }
            });
        }
        "client" => {
            runtime.block_on(async {
                if let Err(e) = run_client().await {
                    eprintln!("Client error: {}", e);
                    std::process::exit(1);
                }
            });
        }
        _ => {
            println!("Well-Known Endpoint Example\n");
            println!("Demonstrates deterministic token addressing (no discovery):\n");
            println!("  - Server: transport.serve_well_known::<PingPong>(token, codec)");
            println!("  - Client: NetTransport::client_well_known::<PingPong>(addr, token, codec)");
            println!("  - Both derive method tokens from the same well-known base\n");
            println!("Usage:");
            println!("  cargo run --example well_known_endpoint -- server");
            println!("  cargo run --example well_known_endpoint -- client\n");
            println!("Run server first in one terminal, then client in another.");
        }
    }
}
