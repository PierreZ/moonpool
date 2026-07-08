//! Well-Known Endpoint Example: Deterministic token addressing.
//!
//! This example demonstrates moonpool's well-known endpoint pattern where
//! both server and client derive tokens from a compile-time constant —
//! no service discovery needed. It also exercises three of the four
//! delivery modes: `get_reply` (the default at-least-once path),
//! `get_reply_unless_failed_for` (failure-aware reply waiting), and
//! `send` (fire-and-forget heartbeats, the natural fit for one-way
//! traffic on well-known endpoints).
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
//! - `#[service]` macro generates a unified `PingPong` struct
//! - `PingPong::well_known(&transport, token_id)` registers at deterministic token (server)
//! - `PingPong::client_well_known(addr, token_id, &transport)` connects without discovery (client)

use std::env;
use std::time::Duration;

use moonpool_transport::{
    NetTransportBuilder, NetworkAddress, Providers, RpcError, TimeProvider, TokioProviders, service,
};
use serde::{Deserialize, Serialize};

// ============================================================================
// Configuration
// ============================================================================

const SERVER_ADDR: &str = "127.0.0.1:4500";
const CLIENT_ADDR: &str = "127.0.0.1:4501";

/// Well-known token for the `PingPong` service.
/// Starts at `FirstAvailable` (3) + 1 to avoid collisions with system tokens.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct HeartbeatRequest {
    seq: u32,
}

// ============================================================================
// Interface Definition
// ============================================================================

#[service]
trait PingPong {
    async fn ping(&self, req: PingRequest) -> Result<PingResponse, RpcError>;
    async fn heartbeat(&self, req: HeartbeatRequest) -> Result<(), RpcError>;
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

    println!("Server listening on {SERVER_ADDR}\n");

    let ping_server = PingPong::well_known(&transport, WLTOKEN_PING_PONG);

    println!(
        "Registered {} method(s) at well-known token {}\n",
        PingPong::METHOD_COUNT,
        WLTOKEN_PING_PONG
    );
    println!("Waiting for ping requests...\n");

    // Two peer streams (ping, heartbeat): fair rotated polling, not `biased;`.
    loop {
        moonpool_core::select! {
            maybe_ping = ping_server.ping.recv() => {
                if let Some((request, reply)) = maybe_ping {
                    println!("Received ping seq={}: {:?}", request.seq, request.message);

                    let response = PingResponse {
                        seq: request.seq,
                        echo: format!("pong: {}", request.message),
                    };

                    reply.send(response.clone());
                    println!("Sent pong seq={}: {:?}\n", response.seq, response.echo);
                } else {
                    println!("Ping stream closed, shutting down.");
                    break;
                }
            }

            maybe_hb = ping_server.heartbeat.recv() => {
                if let Some((request, _reply)) = maybe_hb {
                    // Fire-and-forget: client never awaits a reply.
                    // Dropping `_reply` here is harmless because the client
                    // used `send()` and never registered a `ReplyFuture`.
                    println!("heartbeat seq={}", request.seq);
                } else {
                    println!("Heartbeat stream closed, shutting down.");
                    break;
                }
            }
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

    println!("Connecting to server at {SERVER_ADDR}\n");

    // Client constructed from well-known token — no discovery needed.
    let ping_client = PingPong::client_well_known(server_addr, WLTOKEN_PING_PONG, &transport);

    println!(
        "Using well-known token {} with {} method(s)\n",
        WLTOKEN_PING_PONG,
        PingPong::METHOD_COUNT
    );

    let num_pings = 5;
    let mut success_count = 0;

    for seq in 0..num_pings {
        let request = PingRequest {
            seq,
            message: format!("hello from client (seq={seq})"),
        };

        println!("Sending ping seq={}: {:?}", seq, request.message);

        match ping_client
            .ping
            .get_reply_unless_failed_for(request, Duration::from_secs(5))
            .await
        {
            Ok(response) => {
                println!("Received pong seq={}: {:?}\n", response.seq, response.echo);
                success_count += 1;
            }
            Err(e) => {
                println!("RPC error: {e:?}\n");
            }
        }

        let _ = time.sleep(Duration::from_millis(100)).await;
    }

    println!("=== Results ===");
    println!("{success_count}/{num_pings} pings completed successfully!");

    // Demonstrate fire-and-forget delivery (`send`). Heartbeats are the
    // textbook one-way payload: the client emits liveness signals and never
    // expects a reply. The dropped `ReplyPromise` on the server side is
    // harmless because no `ReplyFuture` was ever registered.
    println!("\n=== Heartbeats (fire-and-forget) ===");
    for seq in 0..3 {
        ping_client.heartbeat.send(HeartbeatRequest { seq })?;
        println!("sent heartbeat {seq}");
        let _ = time.sleep(Duration::from_millis(100)).await;
    }

    Ok(())
}

// ============================================================================
// Main Entry Point
// ============================================================================

fn main() {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map_or("help", std::string::String::as_str);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to create Tokio LocalRuntime");

    match mode {
        "server" => {
            runtime.block_on(async {
                if let Err(e) = run_server().await {
                    eprintln!("Server error: {e}");
                    std::process::exit(1);
                }
            });
        }
        "client" => {
            runtime.block_on(async {
                if let Err(e) = run_client().await {
                    eprintln!("Client error: {e}");
                    std::process::exit(1);
                }
            });
        }
        _ => {
            println!("Well-Known Endpoint Example\n");
            println!("Demonstrates deterministic token addressing (no discovery):\n");
            println!("  - Server: PingPong::well_known(&transport, token)");
            println!("  - Client: PingPong::client_well_known(addr, token, &transport)");
            println!("  - Both derive method tokens from the same well-known base\n");
            println!("Usage:");
            println!("  cargo run --example well_known_endpoint -- server");
            println!("  cargo run --example well_known_endpoint -- client\n");
            println!("Run server first in one terminal, then client in another.");
        }
    }
}
