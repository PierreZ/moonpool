//! Ping-Pong Example: Real TCP RPC using Moonpool's static messaging.
//!
//! This example demonstrates moonpool's RPC over **real TCP sockets**
//! using the improved Phase 12C APIs.
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
//! - `NetTransportBuilder` for clean transport setup
//! - `register_handler` for single-step endpoint registration
//! - `recv_with_transport` for embedded transport in replies
//! - Server: listening, receiving typed requests, sending responses
//! - Client: connecting, sending requests, awaiting responses

use std::env;
use std::time::Duration;

use moonpool_transport::{
    Endpoint, JsonCodec, NetTransportBuilder, NetworkAddress, ReplyFuture, TimeProvider,
    TokioNetworkProvider, TokioTaskProvider, TokioTimeProvider, UID, send_request,
};
use serde::{Deserialize, Serialize};

// ============================================================================
// Configuration
// ============================================================================

const SERVER_ADDR: &str = "127.0.0.1:4500";
const CLIENT_ADDR: &str = "127.0.0.1:4501";

// Well-known token for the ping service
fn ping_token() -> UID {
    UID::new(0x5049_4E47, 0x504F_4E47) // "PING" "PONG" in hex
}

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
// Server
// ============================================================================

async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Ping-Pong Server ===\n");

    // Create providers for real networking
    let network = TokioNetworkProvider::new();
    let time = TokioTimeProvider::new();
    let task = TokioTaskProvider;

    // Parse server address
    let local_addr = NetworkAddress::parse(SERVER_ADDR)?;

    // ========================================================================
    // NetTransportBuilder handles Rc wrapping, set_weak_self(), and listen()
    // No more runtime panics from forgetting set_weak_self()!
    // ========================================================================
    let transport = NetTransportBuilder::new(network, time, task)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Server listening on {}\n", SERVER_ADDR);

    // ========================================================================
    // register_handler combines all endpoint setup into a single call:
    // - Creates endpoint from local address + token
    // - Creates RequestStream
    // - Registers with transport
    // ========================================================================
    let ping_stream = transport.register_handler::<PingRequest, _>(ping_token(), JsonCodec);

    println!("Waiting for ping requests...\n");

    // Server loop - handle requests
    loop {
        // ====================================================================
        // recv_with_transport eliminates the closure callback pattern
        // The transport reference is passed directly instead of captured
        // ====================================================================
        if let Some((request, reply)) = ping_stream
            .recv_with_transport::<_, _, _, PingResponse>(&transport)
            .await
        {
            println!("Received ping seq={}: {:?}", request.seq, request.message);

            // Create and send response
            let response = PingResponse {
                seq: request.seq,
                echo: format!("pong: {}", request.message),
            };

            reply.send(response.clone());
            println!("Sent pong seq={}: {:?}\n", response.seq, response.echo);
        } else {
            // Stream closed
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

    // Create providers for real networking
    let network = TokioNetworkProvider::new();
    let time = TokioTimeProvider::new();
    let task = TokioTaskProvider;

    // Parse addresses
    let local_addr = NetworkAddress::parse(CLIENT_ADDR)?;
    let server_addr = NetworkAddress::parse(SERVER_ADDR)?;

    // ========================================================================
    // Client also uses build_listening() because it needs to receive responses
    // The builder makes this requirement clear in the method name
    // ========================================================================
    let transport = NetTransportBuilder::new(network, time.clone(), task)
        .local_address(local_addr)
        .build_listening()
        .await?;

    // Server endpoint
    let server_endpoint = Endpoint::new(server_addr, ping_token());

    println!("Connecting to server at {}\n", SERVER_ADDR);

    // Send ping requests
    let num_pings = 5;
    let mut success_count = 0;

    for seq in 0..num_pings {
        let request = PingRequest {
            seq,
            message: format!("hello from client (seq={})", seq),
        };

        println!("Sending ping seq={}: {:?}", seq, request.message);

        // Send request and get future for response
        let future: ReplyFuture<PingResponse, JsonCodec> =
            send_request(&transport, &server_endpoint, request, JsonCodec)?;

        // Await response with timeout
        match time.timeout(Duration::from_secs(5), future).await {
            Ok(Ok(Ok(response))) => {
                println!("Received pong seq={}: {:?}\n", response.seq, response.echo);
                success_count += 1;
            }
            Ok(Ok(Err(e))) => {
                println!("RPC error: {:?}\n", e);
            }
            Ok(Err(())) | Err(_) => {
                println!("Timeout waiting for response\n");
            }
        }

        // Small delay between requests
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
    // Parse command line args
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    // Create LocalRuntime (required for spawn_local used by TokioTaskProvider)
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
            println!("Usage:");
            println!("  cargo run --example ping_pong -- server   # Start the server");
            println!("  cargo run --example ping_pong -- client   # Run the client\n");
            println!("Run server first in one terminal, then client in another.");
        }
    }
}
