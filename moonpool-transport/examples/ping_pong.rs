//! Ping-Pong Example: Real TCP RPC using Moonpool's `#[service]` macro.
//!
//! This example demonstrates moonpool's RPC over **real TCP sockets**
//! using the `#[service]` macro and `ServiceEndpoint` delivery modes.
//!
//! Run as two separate processes:
//!
//! ```bash
//! # Terminal 1 - Start the server
//! cargo run --example ping_pong -- server
//!
//! # Terminal 2 - Run the client
//! cargo run --example ping_pong -- client
//! ```
//!
//! # Architecture
//!
//! The example shows:
//! - `#[service]` macro generating Server and Client types
//! - `NetTransportBuilder` for clean transport setup
//! - `ServiceEndpoint` fields with delivery mode methods
//! - Server: listening, receiving typed requests, sending responses
//! - Client: choosing delivery mode per call (get_reply, try_get_reply, send)

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

// ============================================================================
// Message Types
// ============================================================================

/// Request message for ping-pong RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PingRequest {
    /// Sequence number for tracking.
    seq: u32,
    /// Payload message.
    message: String,
}

/// Response message for ping-pong RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PingResponse {
    /// Echoed sequence number.
    seq: u32,
    /// Echoed message with "pong:" prefix.
    echo: String,
}

// ============================================================================
// Interface Definition
// ============================================================================

/// PingPong interface definition.
///
/// The `#[service]` macro generates:
/// - `PingPongServer<C>` with `RequestStream` field (ping)
/// - `PingPongClient` with `ServiceEndpoint` field (ping)
#[service(id = 0x5049_4E47)]
trait PingPong {
    async fn ping(&self, req: PingRequest) -> Result<PingResponse, RpcError>;
}

// ============================================================================
// Server
// ============================================================================

async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Ping-Pong Server ===\n");

    let providers = TokioProviders::new();
    let local_addr = NetworkAddress::parse(SERVER_ADDR)?;

    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Server listening on {}\n", SERVER_ADDR);

    let ping_server = PingPongServer::init(&transport, JsonCodec);

    println!(
        "Registered {} method(s) (interface ID: 0x{:X})\n",
        PingPongServer::<JsonCodec>::METHOD_COUNT,
        PingPongServer::<JsonCodec>::INTERFACE_ID
    );
    println!("Waiting for ping requests...\n");

    loop {
        if let Some((request, reply)) = ping_server
            .ping
            .recv_with_transport::<_, PingResponse>(&transport)
            .await
        {
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
    println!("=== Ping-Pong Client ===\n");

    let providers = TokioProviders::new();
    let time = providers.time().clone();

    let local_addr = NetworkAddress::parse(CLIENT_ADDR)?;
    let server_addr = NetworkAddress::parse(SERVER_ADDR)?;

    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Connecting to server at {}\n", SERVER_ADDR);

    // Client is a plain struct with ServiceEndpoint fields
    let ping_client = PingPongClient::new(server_addr);

    println!(
        "Using interface ID: 0x{:X} with {} method(s)\n",
        PingPongClient::INTERFACE_ID,
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

        // Delivery mode is a call-site decision.
        // Here we use get_reply (at-least-once, reliable):
        match time
            .timeout(
                Duration::from_secs(5),
                ping_client.ping.get_reply(&transport, request, JsonCodec),
            )
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
            println!("Ping-Pong Example: Real TCP RPC with Moonpool\n");
            println!("This example demonstrates the #[service] macro:\n");
            println!("  - PingPongServer<C> with RequestStream field");
            println!("  - PingPongClient with ServiceEndpoint field");
            println!("  - Delivery modes: .get_reply(), .try_get_reply(), .send()\n");
            println!("Usage:");
            println!("  cargo run --example ping_pong -- server   # Start the server");
            println!("  cargo run --example ping_pong -- client   # Run the client\n");
            println!("Run server first in one terminal, then client in another.");
        }
    }
}
