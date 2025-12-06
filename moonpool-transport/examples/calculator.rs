//! Calculator Example: Multi-method RPC interface using Moonpool.
//!
//! This example demonstrates:
//! - `NetTransportBuilder` for cleaner transport setup (Step 9)
//! - `register_handler_at` for multi-method interfaces (Step 10b)
//! - `recv_with_transport` for embedded transport in replies (Step 11)
//! - Using `tokio::select!` to handle multiple request types
//!
//! Run as two separate processes:
//!
//! ```bash
//! # Terminal 1 - Start the server
//! cargo run --example calculator -- server
//!
//! # Terminal 2 - Run the client
//! cargo run --example calculator -- client
//! ```

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

const SERVER_ADDR: &str = "127.0.0.1:4600";
const CLIENT_ADDR: &str = "127.0.0.1:4601";

// Interface and method identifiers
const CALC_INTERFACE: u64 = 0xCA1C_0000;
const METHOD_ADD: u64 = 0;
const METHOD_SUB: u64 = 1;
const METHOD_MUL: u64 = 2;
const METHOD_DIV: u64 = 3;

// ============================================================================
// Message Types
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AddRequest {
    a: i64,
    b: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AddResponse {
    result: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubRequest {
    a: i64,
    b: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubResponse {
    result: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MulRequest {
    a: i64,
    b: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MulResponse {
    result: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DivRequest {
    a: i64,
    b: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DivResponse {
    result: Option<i64>, // None if division by zero
}

// ============================================================================
// Server
// ============================================================================

async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Calculator Server ===\n");

    // Create providers
    let network = TokioNetworkProvider::new();
    let time = TokioTimeProvider::new();
    let task = TokioTaskProvider;

    let local_addr = NetworkAddress::parse(SERVER_ADDR)?;

    // ========================================================================
    // NEW API: NetTransportBuilder eliminates Rc/set_weak_self boilerplate
    // ========================================================================
    let transport = NetTransportBuilder::new(network, time, task)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Server listening on {}\n", SERVER_ADDR);

    // ========================================================================
    // NEW API: register_handler_at for multi-method interfaces
    // Each method gets its own stream with a deterministic token
    // ========================================================================
    let (add_stream, _) =
        transport.register_handler_at::<AddRequest, _>(CALC_INTERFACE, METHOD_ADD, JsonCodec);
    let (sub_stream, _) =
        transport.register_handler_at::<SubRequest, _>(CALC_INTERFACE, METHOD_SUB, JsonCodec);
    let (mul_stream, _) =
        transport.register_handler_at::<MulRequest, _>(CALC_INTERFACE, METHOD_MUL, JsonCodec);
    let (div_stream, _) =
        transport.register_handler_at::<DivRequest, _>(CALC_INTERFACE, METHOD_DIV, JsonCodec);

    println!("Registered 4 calculator methods (add, sub, mul, div)\n");
    println!("Waiting for requests...\n");

    // ========================================================================
    // Using tokio::select! to handle multiple request types concurrently
    // ========================================================================
    loop {
        tokio::select! {
            // ================================================================
            // NEW API: recv_with_transport - no closure callback needed!
            // ================================================================
            Some((req, reply)) = add_stream.recv_with_transport::<_, _, _, AddResponse>(&transport) => {
                let result = req.a + req.b;
                println!("ADD: {} + {} = {}", req.a, req.b, result);
                reply.send(AddResponse { result });
            }

            Some((req, reply)) = sub_stream.recv_with_transport::<_, _, _, SubResponse>(&transport) => {
                let result = req.a - req.b;
                println!("SUB: {} - {} = {}", req.a, req.b, result);
                reply.send(SubResponse { result });
            }

            Some((req, reply)) = mul_stream.recv_with_transport::<_, _, _, MulResponse>(&transport) => {
                let result = req.a * req.b;
                println!("MUL: {} * {} = {}", req.a, req.b, result);
                reply.send(MulResponse { result });
            }

            Some((req, reply)) = div_stream.recv_with_transport::<_, _, _, DivResponse>(&transport) => {
                let result = if req.b != 0 {
                    Some(req.a / req.b)
                } else {
                    None
                };
                match result {
                    Some(r) => println!("DIV: {} / {} = {}", req.a, req.b, r),
                    None => println!("DIV: {} / {} = ERROR (division by zero)", req.a, req.b),
                }
                reply.send(DivResponse { result });
            }
        }
    }
}

// ============================================================================
// Client
// ============================================================================

async fn run_client() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Calculator Client ===\n");

    // Create providers
    let network = TokioNetworkProvider::new();
    let time = TokioTimeProvider::new();
    let task = TokioTaskProvider;

    let local_addr = NetworkAddress::parse(CLIENT_ADDR)?;
    let server_addr = NetworkAddress::parse(SERVER_ADDR)?;

    // Use NetTransportBuilder (same as server)
    let transport = NetTransportBuilder::new(network, time.clone(), task)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Client started, connecting to server at {}\n", SERVER_ADDR);

    // Create endpoints for each method using the same deterministic tokens
    let add_endpoint = Endpoint::new(server_addr.clone(), UID::new(CALC_INTERFACE, METHOD_ADD));
    let sub_endpoint = Endpoint::new(server_addr.clone(), UID::new(CALC_INTERFACE, METHOD_SUB));
    let mul_endpoint = Endpoint::new(server_addr.clone(), UID::new(CALC_INTERFACE, METHOD_MUL));
    let div_endpoint = Endpoint::new(server_addr.clone(), UID::new(CALC_INTERFACE, METHOD_DIV));

    // Test calculations
    let tests = [
        ("10 + 5", Operation::Add(10, 5)),
        ("20 - 7", Operation::Sub(20, 7)),
        ("6 * 8", Operation::Mul(6, 8)),
        ("100 / 4", Operation::Div(100, 4)),
        ("42 / 0", Operation::Div(42, 0)), // Division by zero test
    ];

    let mut success_count = 0;

    for (desc, op) in tests.iter() {
        println!("Sending: {}", desc);

        let result = match op {
            Operation::Add(a, b) => {
                let future: ReplyFuture<AddResponse, JsonCodec> = send_request(
                    &transport,
                    &add_endpoint,
                    AddRequest { a: *a, b: *b },
                    JsonCodec,
                )?;

                match time.timeout(Duration::from_secs(5), future).await {
                    Ok(Ok(Ok(resp))) => Some(format!("{}", resp.result)),
                    _ => None,
                }
            }
            Operation::Sub(a, b) => {
                let future: ReplyFuture<SubResponse, JsonCodec> = send_request(
                    &transport,
                    &sub_endpoint,
                    SubRequest { a: *a, b: *b },
                    JsonCodec,
                )?;

                match time.timeout(Duration::from_secs(5), future).await {
                    Ok(Ok(Ok(resp))) => Some(format!("{}", resp.result)),
                    _ => None,
                }
            }
            Operation::Mul(a, b) => {
                let future: ReplyFuture<MulResponse, JsonCodec> = send_request(
                    &transport,
                    &mul_endpoint,
                    MulRequest { a: *a, b: *b },
                    JsonCodec,
                )?;

                match time.timeout(Duration::from_secs(5), future).await {
                    Ok(Ok(Ok(resp))) => Some(format!("{}", resp.result)),
                    _ => None,
                }
            }
            Operation::Div(a, b) => {
                let future: ReplyFuture<DivResponse, JsonCodec> = send_request(
                    &transport,
                    &div_endpoint,
                    DivRequest { a: *a, b: *b },
                    JsonCodec,
                )?;

                match time.timeout(Duration::from_secs(5), future).await {
                    Ok(Ok(Ok(resp))) => Some(
                        resp.result
                            .map(|r| r.to_string())
                            .unwrap_or_else(|| "ERROR (division by zero)".to_string()),
                    ),
                    _ => None,
                }
            }
        };

        match result {
            Some(r) => {
                println!("  Result: {}\n", r);
                success_count += 1;
            }
            None => {
                println!("  ERROR: Request failed or timed out\n");
            }
        }

        // Small delay between requests
        let _ = time.sleep(Duration::from_millis(100)).await;
    }

    println!("=== Results ===");
    println!(
        "{}/{} operations completed successfully!",
        success_count,
        tests.len()
    );

    Ok(())
}

enum Operation {
    Add(i64, i64),
    Sub(i64, i64),
    Mul(i64, i64),
    Div(i64, i64),
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
            println!("Calculator Example: Multi-method RPC with Moonpool\n");
            println!("This example demonstrates the improved Phase 12C APIs:\n");
            println!("  - NetTransportBuilder (eliminates Rc/set_weak_self boilerplate)");
            println!("  - register_handler_at (multi-method interface registration)");
            println!("  - recv_with_transport (no closure callback needed)\n");
            println!("Usage:");
            println!("  cargo run --example calculator -- server   # Start the server");
            println!("  cargo run --example calculator -- client   # Run the client\n");
            println!("Run server first in one terminal, then client in another.");
        }
    }
}
