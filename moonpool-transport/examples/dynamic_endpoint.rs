//! Dynamic Endpoint Example: Runtime-allocated tokens with full-interface serialization.
//!
//! This example demonstrates moonpool's "interfaces are data" pattern: the entire
//! `Calculator` interface is serialized as a compact `{ address, base_token }` blob.
//! The client reconstructs every method's endpoint via offset adjustment from the
//! base token — see the FDB analysis at `docs/analysis/foundationdb/layer-3-fdbrpc.md`
//! section 7.
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
//! - `#[service]` macro generates a unified `Calculator` struct that implements
//!   `Serialize` and exposes `Calculator::deserialize_with(&transport, deserializer)`.
//! - Server: `Calculator::init(&transport)` allocates a random base token,
//!   then `serde_json::to_string(&calc)` emits `{ address, base_token }`.
//! - Client: `Calculator::deserialize_with(&transport, &mut Deserializer)` rebuilds
//!   the unified struct in remote mode, with each method's endpoint at
//!   `base_token.adjusted(idx)`.
//! - Two `Calculator` servers in the same process get different tokens (no collisions).

use std::env;
use std::time::Duration;

use moonpool_transport::{NetTransportBuilder, NetworkAddress, RpcError, TokioProviders, service};
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
    let calc = Calculator::init(&transport);

    println!(
        "Registered {} methods with dynamic base token: {}\n",
        Calculator::METHOD_COUNT,
        calc.base_token(),
    );

    // Demonstrate: two servers in the same process get different tokens
    let calc2 = Calculator::init(&transport);
    println!(
        "Second server base token: {} (different!)\n",
        calc2.base_token()
    );
    assert_ne!(calc.base_token(), calc2.base_token());

    // Serialize the whole interface — the client reconstructs every method
    // endpoint via offset adjustment from the base token.
    let iface_json = serde_json::to_string(&calc)?;
    println!("=== COPY-PASTE THIS TO START THE CLIENT ===\n");
    println!(
        "  cargo run --example dynamic_endpoint -- client '{}'\n",
        iface_json
    );
    println!("=============================================\n");

    println!("Waiting for requests...\n");

    // Handle requests (only from the first server for this demo)
    loop {
        tokio::select! {
            Some((req, reply)) = calc.add.recv() => {
                let result = req.a + req.b;
                println!("ADD: {} + {} = {}", req.a, req.b, result);
                reply.send(AddResponse { result });
            }

            Some((req, reply)) = calc.sub.recv() => {
                let result = req.a - req.b;
                println!("SUB: {} - {} = {}", req.a, req.b, result);
                reply.send(SubResponse { result });
            }

            Some((req, reply)) = calc.mul.recv() => {
                let result = req.a * req.b;
                println!("MUL: {} * {} = {}", req.a, req.b, result);
                reply.send(MulResponse { result });
            }

            Some((req, reply)) = calc.div.recv() => {
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

async fn run_client(iface_json: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Dynamic Endpoint Client ===\n");

    let providers = TokioProviders::new();

    let local_addr = NetworkAddress::parse(CLIENT_ADDR)?;

    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr)
        .build_listening()
        .await?;

    // Reconstruct the whole interface from the JSON blob (simulating discovery).
    // The address and base token come from the wire; deserialize_with binds
    // each method endpoint to the local transport.
    let mut de = serde_json::Deserializer::from_str(iface_json);
    let calc = Calculator::deserialize_with(&transport, &mut de)?;
    println!(
        "Reconstructed interface: address={}, base_token={}\n",
        calc.address(),
        calc.base_token(),
    );

    println!("Client started, connecting to server at {}\n", SERVER_ADDR);

    // Delivery mode is a call-site decision (FDB pattern)
    println!("--- get_reply_unless_failed_for (at-least-once) ---");
    match calc
        .add
        .get_reply_unless_failed_for(AddRequest { a: 10, b: 5 }, Duration::from_secs(5))
        .await
    {
        Ok(resp) => println!("  10 + 5 = {}\n", resp.result),
        Err(e) => println!("  Failed: {:?}\n", e),
    }

    println!("--- try_get_reply (at-most-once) ---");
    match calc.sub.try_get_reply(SubRequest { a: 20, b: 7 }).await {
        Ok(resp) => println!("  20 - 7 = {}\n", resp.result),
        Err(e) if e.is_maybe_delivered() => {
            println!("  MaybeDelivered — read state before retry\n")
        }
        Err(e) => println!("  Error: {:?}\n", e),
    }

    println!("--- get_reply_unless_failed_for ---");
    match calc
        .mul
        .get_reply_unless_failed_for(MulRequest { a: 6, b: 8 }, Duration::from_secs(5))
        .await
    {
        Ok(resp) => println!("  6 * 8 = {}\n", resp.result),
        Err(e) => println!("  Failed: {:?}\n", e),
    }

    println!("--- division ---");
    match calc
        .div
        .get_reply_unless_failed_for(DivRequest { a: 100, b: 4 }, Duration::from_secs(5))
        .await
    {
        Ok(resp) => println!(
            "  100 / 4 = {}\n",
            resp.result
                .map(|r| r.to_string())
                .unwrap_or_else(|| "ERROR".to_string())
        ),
        Err(e) => println!("  Failed: {:?}\n", e),
    }

    match calc
        .div
        .get_reply_unless_failed_for(DivRequest { a: 42, b: 0 }, Duration::from_secs(5))
        .await
    {
        Ok(resp) => println!(
            "  42 / 0 = {}\n",
            resp.result
                .map(|r| r.to_string())
                .unwrap_or_else(|| "ERROR (division by zero)".to_string())
        ),
        Err(e) => println!("  Failed: {:?}\n", e),
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
            let iface_json = args
                .get(2)
                .expect("Usage: dynamic_endpoint client '<iface_json>'");
            runtime.block_on(async {
                if let Err(e) = run_client(iface_json).await {
                    eprintln!("Client error: {}", e);
                    std::process::exit(1);
                }
            });
        }
        _ => {
            println!("Dynamic Endpoint Example\n");
            println!("Demonstrates runtime-allocated tokens with full-interface serialization:\n");
            println!("  - Server: Calculator::init(&transport) → random base token");
            println!("           serde_json::to_string(&calc) → {{ address, base_token }} blob");
            println!(
                "  - Client: Calculator::deserialize_with(&transport, deserializer) → remote interface"
            );
            println!("  - Two servers in same process get different tokens (no collisions)\n");
            println!("Usage:");
            println!("  cargo run --example dynamic_endpoint -- server");
            println!("  cargo run --example dynamic_endpoint -- client '<iface_json>'\n");
            println!("Run server first, then copy the interface JSON to the client command.");
        }
    }
}
