//! Ping-Pong Example: Real TCP RPC using Moonpool's `#[interface]` macro.
//!
//! This example demonstrates moonpool's RPC over **real TCP sockets**
//! using the ergonomic `#[interface]` macro and bound client pattern.
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
//! - `#[interface]` macro for generating Server, Client, and BoundClient types
//! - `NetTransportBuilder` for clean transport setup
//! - `.bind()` pattern for Orleans-style ergonomic client calls
//! - Server: listening, receiving typed requests, sending responses
//! - Client: direct trait method calls that feel like local function calls

use std::env;
use std::time::Duration;

use moonpool_transport::{
    JsonCodec, NetTransportBuilder, NetworkAddress, Providers, RpcError, TimeProvider,
    TokioProviders, interface,
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
/// The `#[interface]` macro generates:
/// - `PingPongServer<C>` with `RequestStream` field (ping)
/// - `PingPongClient` with `bind()` method
/// - `BoundPingPongClient<N, T, TP, C>` implementing the `PingPong` trait
#[interface(id = 0x5049_4E47)]
trait PingPong {
    async fn ping(&self, req: PingRequest) -> Result<PingResponse, RpcError>;
}

// ============================================================================
// Server
// ============================================================================

async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Ping-Pong Server ===\n");

    // Create providers bundle for real networking
    let providers = TokioProviders::new();

    // Parse server address
    let local_addr = NetworkAddress::parse(SERVER_ADDR)?;

    // ========================================================================
    // NetTransportBuilder handles Rc wrapping, set_weak_self(), and listen()
    // No more runtime panics from forgetting set_weak_self()!
    // ========================================================================
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Server listening on {}\n", SERVER_ADDR);

    // ========================================================================
    // Single line registers the handler using the interface macro
    // ========================================================================
    let ping_server = PingPongServer::init(&transport, JsonCodec);

    println!(
        "Registered {} method(s) (interface ID: 0x{:X})\n",
        PingPongServer::<JsonCodec>::METHOD_COUNT,
        PingPongServer::<JsonCodec>::INTERFACE_ID
    );
    println!("Waiting for ping requests...\n");

    // Server loop - handle requests
    loop {
        // ====================================================================
        // recv_with_transport eliminates the closure callback pattern
        // The transport reference is passed directly instead of captured
        // ====================================================================
        if let Some((request, reply)) = ping_server
            .ping
            .recv_with_transport::<_, PingResponse>(&transport)
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

    // Create providers bundle for real networking
    let providers = TokioProviders::new();
    let time = providers.time().clone();

    // Parse addresses
    let local_addr = NetworkAddress::parse(CLIENT_ADDR)?;
    let server_addr = NetworkAddress::parse(SERVER_ADDR)?;

    // ========================================================================
    // Client also uses build_listening() because it needs to receive responses
    // The builder makes this requirement clear in the method name
    // ========================================================================
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Connecting to server at {}\n", SERVER_ADDR);

    // ========================================================================
    // NEW: Ergonomic bound client - implements PingPong trait!
    // ========================================================================
    let ping_client = PingPongClient::new(server_addr).bind(transport.clone(), JsonCodec);

    println!(
        "Using interface ID: 0x{:X} with {} method(s)\n",
        PingPongClient::INTERFACE_ID,
        PingPongClient::METHOD_COUNT
    );

    // Send ping requests
    let num_pings = 5;
    let mut success_count = 0;

    for seq in 0..num_pings {
        let request = PingRequest {
            seq,
            message: format!("hello from client (seq={})", seq),
        };

        println!("Sending ping seq={}: {:?}", seq, request.message);

        // ====================================================================
        // NEW: Clean trait method call with .await
        // No more manual send_request or type annotations!
        // Note: timeout returns SimulationResult<Result<T, ()>> so we have 3 levels
        // ====================================================================
        match time
            .timeout(Duration::from_secs(5), ping_client.ping(request))
            .await
        {
            Ok(Ok(Ok(response))) => {
                println!("Received pong seq={}: {:?}\n", response.seq, response.echo);
                success_count += 1;
            }
            Ok(Ok(Err(e))) => {
                println!("RPC error: {:?}\n", e);
            }
            Ok(Err(())) => {
                println!("Timeout waiting for response\n");
            }
            Err(e) => {
                println!("Simulation error: {:?}\n", e);
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
            println!("This example demonstrates the #[interface] macro:\n");
            println!("  - PingPongServer<C> with RequestStream field");
            println!("  - PingPongClient with .bind() for creating BoundPingPongClient");
            println!("  - BoundPingPongClient implements PingPong trait for ergonomic calls");
            println!("  - Clean syntax: ping_client.ping(request).await?\n");
            println!("Usage:");
            println!("  cargo run --example ping_pong -- server   # Start the server");
            println!("  cargo run --example ping_pong -- client   # Run the client\n");
            println!("Run server first in one terminal, then client in another.");
        }
    }
}
