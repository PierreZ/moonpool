//! Ping-Pong Example: Real TCP RPC using Moonpool's static messaging.
//!
//! This example demonstrates moonpool's RPC over **real TCP sockets**.
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
//! - Setting up a FlowTransport with real Tokio networking
//! - Server: listening, receiving typed requests, sending responses
//! - Client: connecting, sending requests, awaiting responses
//!
//! # Current API Pain Points (demonstrated in comments)
//!
//! 1. Must wrap FlowTransport in Rc and call set_weak_self() manually
//! 2. RequestStream::recv() requires a closure callback for sending
//! 3. Multiple steps to register an endpoint (UID + Endpoint + RequestStream + register)
//!
//! See the end of this file for the planned improved API.

use std::env;
use std::rc::Rc;
use std::time::Duration;

use moonpool::{
    Endpoint, FlowTransport, MessageReceiver, NetworkAddress, ReplyFuture, RequestStream, UID,
    send_request,
};
use moonpool_foundation::{
    TimeProvider, TokioNetworkProvider, TokioTaskProvider, TokioTimeProvider,
};
use moonpool_traits::JsonCodec;
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
    // PAIN POINT 1: Manual Rc wrapping and set_weak_self()
    // If you forget set_weak_self(), you get a runtime panic!
    // ========================================================================
    let transport = Rc::new(FlowTransport::new(
        local_addr.clone(),
        network,
        time.clone(),
        task.clone(),
    ));
    transport.set_weak_self(Rc::downgrade(&transport));

    // Start listening for incoming connections
    transport.listen().await?;
    println!("Server listening on {}\n", SERVER_ADDR);

    // ========================================================================
    // PAIN POINT 2: Multiple steps to create an endpoint
    // - Create UID manually
    // - Create Endpoint from address + UID
    // - Create RequestStream with endpoint
    // - Cast queue to dyn MessageReceiver
    // - Register with transport
    // ========================================================================
    let server_endpoint = Endpoint::new(local_addr.clone(), ping_token());
    let ping_stream: RequestStream<PingRequest, JsonCodec> =
        RequestStream::new(server_endpoint.clone(), JsonCodec);
    transport.register(ping_token(), ping_stream.queue() as Rc<dyn MessageReceiver>);

    println!("Waiting for ping requests...\n");

    // Server loop - handle requests
    loop {
        // ====================================================================
        // PAIN POINT 3: Closure callback pattern for sending replies
        // The closure captures the transport to send the response back.
        // This is awkward and error-prone.
        // ====================================================================
        let transport_clone = transport.clone();
        if let Some((request, reply)) = ping_stream
            .recv::<_, PingResponse>(move |endpoint, payload| {
                // This closure is called when reply.send() is invoked
                let _ = transport_clone.send_reliable(endpoint, payload);
            })
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

    // Create FlowTransport for client (same boilerplate as server)
    let transport = Rc::new(FlowTransport::new(
        local_addr.clone(),
        network,
        time.clone(),
        task.clone(),
    ));
    transport.set_weak_self(Rc::downgrade(&transport));

    // ========================================================================
    // PAIN POINT 4: Client must call listen() to receive responses!
    // This is NOT obvious - you'd think only servers need listen().
    // Without this, responses from the server never get received.
    // ========================================================================
    transport.listen().await?;

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
        // timeout returns SimulationResult<Result<T, ()>> where T is the future's output
        // ReplyFuture returns Result<PingResponse, ReplyError>
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

// ============================================================================
// Ideal API (Future Improvement)
// ============================================================================
//
// The current API has several pain points. Here's what an improved API would
// look like:
//
// ```rust,ignore
// // === IMPROVED SERVER ===
// let server = FlowTransportBuilder::new(SERVER_ADDR)
//     .with_tokio()                    // Auto-creates Tokio providers
//     .build_listening()               // Returns Rc, calls set_weak_self(), starts listen()
//     .await?;
//
// // Single method to register typed endpoint
// let ping = server.register_endpoint::<PingRequest, PingResponse>(ping_token())?;
//
// loop {
//     // No closure needed - transport ref embedded in ReplyPromise
//     let (request, reply) = ping.recv().await?;
//     reply.send(PingResponse { echo: format!("pong: {}", request.message) });
// }
//
// // === IMPROVED CLIENT ===
// let client = FlowTransportBuilder::new(CLIENT_ADDR)
//     .with_tokio()
//     .build()?;
//
// // Simpler request API with built-in timeout
// let response = client
//     .request(&server_endpoint, PingRequest { seq: 0, message: "hello".into() })
//     .timeout(Duration::from_secs(5))
//     .await?;
// ```
//
// Key improvements:
// 1. FlowTransportBuilder eliminates set_weak_self() footgun
// 2. with_tokio() auto-creates all providers
// 3. build_listening() combines Rc wrap + set_weak_self + listen
// 4. register_endpoint() combines UID + Endpoint + RequestStream + register
// 5. ReplyPromise has embedded transport ref (no closure needed)
// 6. Built-in timeout support on requests
