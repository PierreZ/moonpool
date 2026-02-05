//! Calculator Example: Multi-method RPC interface using Moonpool.
//!
//! This example demonstrates the `#[interface]` attribute macro that generates
//! server and client interface structs from a trait definition.
//!
//! # Key Features
//!
//! - `#[interface(id = ...)]` generates `CalculatorServer`, `CalculatorClient`, and `BoundCalculatorClient`
//! - Trait-based interface definition (idiomatic Rust)
//! - Orleans-style ergonomic client calls via `.bind()`
//! - Only base endpoint is serialized (FDB pattern)
//!
//! # Run
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
    JsonCodec, NetTransportBuilder, NetworkAddress, Providers, RpcError, TimeProvider,
    TokioProviders, interface,
};
use serde::{Deserialize, Serialize};

// ============================================================================
// Configuration
// ============================================================================

const SERVER_ADDR: &str = "127.0.0.1:4600";
const CLIENT_ADDR: &str = "127.0.0.1:4601";

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
// Interface Definition - Generated Server and Client Types
// ============================================================================

/// Calculator interface definition.
///
/// The `#[interface]` macro generates:
/// - `CalculatorServer<C>` with `RequestStream` fields (add, sub, mul, div)
/// - `CalculatorClient` with `bind()` method for creating bound clients
/// - `BoundCalculatorClient<N, T, TP, C>` implementing the `Calculator` trait
#[interface(id = 0xCA1C_0000)]
trait Calculator {
    async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError>;
    async fn sub(&self, req: SubRequest) -> Result<SubResponse, RpcError>;
    async fn mul(&self, req: MulRequest) -> Result<MulResponse, RpcError>;
    async fn div(&self, req: DivRequest) -> Result<DivResponse, RpcError>;
}

// ============================================================================
// Server
// ============================================================================

async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Calculator Server ===\n");

    // Create providers bundle
    let providers = TokioProviders::new();

    let local_addr = NetworkAddress::parse(SERVER_ADDR)?;

    // Build transport
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Server listening on {}\n", SERVER_ADDR);

    // ========================================================================
    // Single line registers all 4 handlers
    // ========================================================================
    let calc = CalculatorServer::init(&transport, JsonCodec);

    println!(
        "Registered {} calculator methods (interface ID: 0x{:X})\n",
        CalculatorServer::<JsonCodec>::METHOD_COUNT,
        CalculatorServer::<JsonCodec>::INTERFACE_ID
    );
    println!("Waiting for requests...\n");

    // Handle requests
    loop {
        tokio::select! {
            Some((req, reply)) = calc.add.recv_with_transport::<_, AddResponse>(&transport) => {
                let result = req.a + req.b;
                println!("ADD: {} + {} = {}", req.a, req.b, result);
                reply.send(AddResponse { result });
            }

            Some((req, reply)) = calc.sub.recv_with_transport::<_, SubResponse>(&transport) => {
                let result = req.a - req.b;
                println!("SUB: {} - {} = {}", req.a, req.b, result);
                reply.send(SubResponse { result });
            }

            Some((req, reply)) = calc.mul.recv_with_transport::<_, MulResponse>(&transport) => {
                let result = req.a * req.b;
                println!("MUL: {} * {} = {}", req.a, req.b, result);
                reply.send(MulResponse { result });
            }

            Some((req, reply)) = calc.div.recv_with_transport::<_, DivResponse>(&transport) => {
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

    // Create providers bundle
    let providers = TokioProviders::new();
    let time = providers.time().clone();

    let local_addr = NetworkAddress::parse(CLIENT_ADDR)?;
    let server_addr = NetworkAddress::parse(SERVER_ADDR)?;

    // Build transport
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Client started, connecting to server at {}\n", SERVER_ADDR);

    // ========================================================================
    // NEW: Ergonomic bound client - implements Calculator trait!
    // ========================================================================
    let calc = CalculatorClient::new(server_addr).bind(transport.clone(), JsonCodec);

    println!(
        "Using interface ID: 0x{:X} with {} methods\n",
        CalculatorClient::INTERFACE_ID,
        CalculatorClient::METHOD_COUNT
    );

    // Test calculations - now with clean trait method calls!
    let tests = [
        ("10 + 5", Op::Add(10, 5)),
        ("20 - 7", Op::Sub(20, 7)),
        ("6 * 8", Op::Mul(6, 8)),
        ("100 / 4", Op::Div(100, 4)),
        ("42 / 0", Op::Div(42, 0)), // Division by zero test
    ];

    let mut success_count = 0;

    for (desc, op) in tests.iter() {
        println!("Sending: {}", desc);

        // NEW: Clean trait method calls with .await?
        // Note: timeout returns Result<T, TimeError>, inner T is Result<Response, RpcError>
        let result = match op {
            Op::Add(a, b) => {
                match time
                    .timeout(
                        Duration::from_secs(5),
                        calc.add(AddRequest { a: *a, b: *b }),
                    )
                    .await
                {
                    Ok(Ok(resp)) => Some(format!("{}", resp.result)),
                    _ => None,
                }
            }
            Op::Sub(a, b) => {
                match time
                    .timeout(
                        Duration::from_secs(5),
                        calc.sub(SubRequest { a: *a, b: *b }),
                    )
                    .await
                {
                    Ok(Ok(resp)) => Some(format!("{}", resp.result)),
                    _ => None,
                }
            }
            Op::Mul(a, b) => {
                match time
                    .timeout(
                        Duration::from_secs(5),
                        calc.mul(MulRequest { a: *a, b: *b }),
                    )
                    .await
                {
                    Ok(Ok(resp)) => Some(format!("{}", resp.result)),
                    _ => None,
                }
            }
            Op::Div(a, b) => {
                match time
                    .timeout(
                        Duration::from_secs(5),
                        calc.div(DivRequest { a: *a, b: *b }),
                    )
                    .await
                {
                    Ok(Ok(resp)) => Some(
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

    // Demonstrate serialization (only base endpoint is serialized)
    println!("\n=== Serialization Demo ===");
    let unbound = CalculatorClient::new(NetworkAddress::parse(SERVER_ADDR)?);
    let json = serde_json::to_string_pretty(&unbound)?;
    println!(
        "Serialized CalculatorClient (only base endpoint):\n{}",
        json
    );

    Ok(())
}

enum Op {
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
            println!("Calculator Example: FDB-style Interface Pattern\n");
            println!("This example demonstrates the #[interface] macro:\n");
            println!(
                "  - Single interface definition generates Server, Client, and BoundClient types"
            );
            println!("  - CalculatorServer<C> with RequestStream fields");
            println!("  - CalculatorClient with .bind() for creating BoundCalculatorClient");
            println!("  - BoundCalculatorClient implements Calculator trait for ergonomic calls");
            println!("  - Only base endpoint is serialized (FDB pattern)\n");
            println!("Usage:");
            println!("  cargo run --example calculator -- server   # Start the server");
            println!("  cargo run --example calculator -- client   # Run the client\n");
            println!("Run server first in one terminal, then client in another.");
        }
    }
}
