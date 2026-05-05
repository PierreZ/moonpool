//! Dynamic Endpoint Example: Runtime-allocated tokens with serialization.
//!
//! This example demonstrates moonpool's dynamic endpoint pattern where
//! tokens are allocated at runtime, and clients discover the interface
//! via serialization (simulating service discovery).
//!
//! Run as two separate processes:
//!
//! ```bash
//! # Terminal 1 - Start the server (prints serialized interface)
//! cargo run --example dynamic_endpoint -- server
//!
//! # Terminal 2 - Run the client (paste the serialized interface)
//! cargo run --example dynamic_endpoint -- client '<json from server>'
//! ```
//!
//! # Architecture
//!
//! - `#[service]` macro generates Server and Client types (no hardcoded ID)
//! - Server: `CalculatorServer::init(&transport, codec)` allocates random base token
//! - Client: `CalculatorClient::from_base(addr, base_token, codec)` reconstructs endpoints
//! - Two Calculator servers in the same process get different tokens (no collisions)

use std::env;
use std::time::Duration;

use moonpool_transport::{
    JsonCodec, NetTransportBuilder, NetworkAddress, Providers, RpcError, TimeProvider,
    TokioProviders, UID, service,
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
    result: Option<i64>,
}

// ============================================================================
// Interface Definition (no id = dynamic tokens)
// ============================================================================

#[service]
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
    println!("=== Dynamic Endpoint Server ===\n");

    let providers = TokioProviders::new();
    let local_addr = NetworkAddress::parse(SERVER_ADDR)?;

    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Server listening on {}\n", SERVER_ADDR);

    // Dynamic token allocation — each init() gets a unique base token.
    let calc = CalculatorServer::init(&transport, JsonCodec);
    let base_token = calc.base_token();

    println!(
        "Registered {} methods with dynamic base token: {}\n",
        CalculatorServer::<JsonCodec>::METHOD_COUNT,
        base_token,
    );

    // Demonstrate: two servers in the same process get different tokens
    let calc2 = CalculatorServer::init(&transport, JsonCodec);
    println!(
        "Second server base token: {} (different!)\n",
        calc2.base_token()
    );
    assert_ne!(calc.base_token(), calc2.base_token());

    // Print the base token for the client to use
    let token_json = serde_json::to_string(&base_token)?;
    println!("=== COPY-PASTE THIS TO START THE CLIENT ===\n");
    println!(
        "  cargo run --example dynamic_endpoint -- client '{}'\n",
        token_json
    );
    println!("=============================================\n");

    println!("Waiting for requests...\n");

    // Handle requests (only from the first server for this demo)
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

async fn run_client(base_token_json: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Dynamic Endpoint Client ===\n");

    let providers = TokioProviders::new();
    let time = providers.time().clone();

    let local_addr = NetworkAddress::parse(CLIENT_ADDR)?;
    let server_addr = NetworkAddress::parse(SERVER_ADDR)?;

    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await?;

    // Reconstruct client from base token (simulating discovery)
    let base_token: UID = serde_json::from_str(base_token_json)?;
    println!("Using discovered base token: {}\n", base_token);

    let calc = CalculatorClient::from_base(server_addr, base_token, JsonCodec);

    println!("Client started, connecting to server at {}\n", SERVER_ADDR);

    // Delivery mode is a call-site decision (FDB pattern)
    println!("--- get_reply (at-least-once) ---");
    match time
        .timeout(
            Duration::from_secs(5),
            calc.add.get_reply(&transport, AddRequest { a: 10, b: 5 }),
        )
        .await
    {
        Ok(Ok(resp)) => println!("  10 + 5 = {}\n", resp.result),
        other => println!("  Failed: {:?}\n", other),
    }

    println!("--- try_get_reply (at-most-once) ---");
    match calc
        .sub
        .try_get_reply(&transport, SubRequest { a: 20, b: 7 })
        .await
    {
        Ok(resp) => println!("  20 - 7 = {}\n", resp.result),
        Err(e) if e.is_maybe_delivered() => {
            println!("  MaybeDelivered — read state before retry\n")
        }
        Err(e) => println!("  Error: {:?}\n", e),
    }

    println!("--- get_reply ---");
    match time
        .timeout(
            Duration::from_secs(5),
            calc.mul.get_reply(&transport, MulRequest { a: 6, b: 8 }),
        )
        .await
    {
        Ok(Ok(resp)) => println!("  6 * 8 = {}\n", resp.result),
        other => println!("  Failed: {:?}\n", other),
    }

    println!("--- division ---");
    match time
        .timeout(
            Duration::from_secs(5),
            calc.div.get_reply(&transport, DivRequest { a: 100, b: 4 }),
        )
        .await
    {
        Ok(Ok(resp)) => println!(
            "  100 / 4 = {}\n",
            resp.result
                .map(|r| r.to_string())
                .unwrap_or_else(|| "ERROR".to_string())
        ),
        other => println!("  Failed: {:?}\n", other),
    }

    match time
        .timeout(
            Duration::from_secs(5),
            calc.div.get_reply(&transport, DivRequest { a: 42, b: 0 }),
        )
        .await
    {
        Ok(Ok(resp)) => println!(
            "  42 / 0 = {}\n",
            resp.result
                .map(|r| r.to_string())
                .unwrap_or_else(|| "ERROR (division by zero)".to_string())
        ),
        other => println!("  Failed: {:?}\n", other),
    }

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
            let token_json = args
                .get(2)
                .expect("Usage: dynamic_endpoint client '<base_token_json>'");
            runtime.block_on(async {
                if let Err(e) = run_client(token_json).await {
                    eprintln!("Client error: {}", e);
                    std::process::exit(1);
                }
            });
        }
        _ => {
            println!("Dynamic Endpoint Example\n");
            println!("Demonstrates runtime-allocated tokens with discovery:\n");
            println!("  - Server: CalculatorServer::init(&transport, codec) → random base token");
            println!("  - Client: CalculatorClient::from_base(addr, base_token, codec)");
            println!("  - Two servers in same process get different tokens (no collisions)\n");
            println!("Usage:");
            println!("  cargo run --example dynamic_endpoint -- server");
            println!("  cargo run --example dynamic_endpoint -- client '<base_token_json>'\n");
            println!("Run server first, then copy the base token JSON to the client command.");
        }
    }
}
