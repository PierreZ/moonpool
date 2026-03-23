//! Calculator Example: Multi-method RPC interface using Moonpool.
//!
//! This example demonstrates the `#[service]` attribute macro that generates
//! server and client interface structs from a trait definition.
//!
//! # Key Features
//!
//! - `#[service(id = ...)]` generates `CalculatorServer` and `CalculatorClient`
//! - Client has `ServiceEndpoint` fields with delivery mode methods
//! - Each call site chooses its delivery mode (FDB pattern)
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
    TokioProviders, service,
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
// Interface Definition
// ============================================================================

/// Calculator interface definition.
///
/// The `#[service]` macro generates:
/// - `CalculatorServer<C>` with `RequestStream` fields (add, sub, mul, div)
/// - `CalculatorClient` with `ServiceEndpoint` fields (add, sub, mul, div)
#[service(id = 0xCA1C_0000)]
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

    let providers = TokioProviders::new();
    let local_addr = NetworkAddress::parse(SERVER_ADDR)?;

    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Server listening on {}\n", SERVER_ADDR);

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

    let providers = TokioProviders::new();
    let time = providers.time().clone();

    let local_addr = NetworkAddress::parse(CLIENT_ADDR)?;
    let server_addr = NetworkAddress::parse(SERVER_ADDR)?;

    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Client started, connecting to server at {}\n", SERVER_ADDR);

    // Client is a plain struct with ServiceEndpoint fields.
    // Codec is passed once at construction; delivery methods don't need it.
    let calc = CalculatorClient::new(server_addr, JsonCodec);

    println!(
        "Using interface ID: 0x{:X} with {} methods\n",
        CalculatorClient::<JsonCodec>::INTERFACE_ID,
        CalculatorClient::<JsonCodec>::METHOD_COUNT
    );

    // ========================================================================
    // Delivery mode is a call-site decision (FDB pattern)
    // ========================================================================

    // 1. get_reply: at-least-once, reliable delivery (default for most RPCs)
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

    // 2. try_get_reply: at-most-once, races reply vs disconnect
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

    // 3. get_reply again for mul
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

    // 4. Division (normal + division by zero)
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

    // Demonstrate serialization (client is just a struct of ServiceEndpoints)
    println!("=== Serialization Demo ===");
    let unbound = CalculatorClient::new(NetworkAddress::parse(SERVER_ADDR)?, JsonCodec);
    let json = serde_json::to_string_pretty(&unbound)?;
    println!("Serialized CalculatorClient:\n{}", json);

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
            println!("Calculator Example: FDB-style Interface Pattern\n");
            println!("This example demonstrates the #[service] macro:\n");
            println!("  - CalculatorServer<C> with RequestStream fields");
            println!("  - CalculatorClient with ServiceEndpoint fields");
            println!("  - Delivery modes chosen per call: get_reply, try_get_reply, send");
            println!("  - FDB pattern: calc.add.get_reply(&transport, req, codec).await?\n");
            println!("Usage:");
            println!("  cargo run --example calculator -- server   # Start the server");
            println!("  cargo run --example calculator -- client   # Run the client\n");
            println!("Run server first in one terminal, then client in another.");
        }
    }
}
