//! Calculator Example: Multi-method RPC interface using Moonpool.
//!
//! This example demonstrates:
//! - `rpc_messages!` macro for message type definitions
//! - `rpc_interface!` macro for interface token generation
//! - `NetTransportBuilder` for cleaner transport setup
//! - `transport.request().with_timeout().send()` fluent builder for RPC calls
//! - `register_handler_at` for multi-method interfaces
//! - `recv_with_transport` for embedded transport in replies
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
    JsonCodec, NetTransportBuilder, NetworkAddress, TimeProvider, TokioNetworkProvider,
    TokioTaskProvider, TokioTimeProvider, rpc_interface, rpc_messages,
};

// ============================================================================
// Configuration
// ============================================================================

const SERVER_ADDR: &str = "127.0.0.1:4600";
const CLIENT_ADDR: &str = "127.0.0.1:4601";

// ============================================================================
// Message Types - using rpc_messages! macro
// ============================================================================

rpc_messages! {
    /// Request to add two numbers
    pub struct AddRequest {
        pub a: i64,
        pub b: i64,
    }

    /// Response with the sum
    pub struct AddResponse {
        pub result: i64,
    }

    /// Request to subtract two numbers
    pub struct SubRequest {
        pub a: i64,
        pub b: i64,
    }

    /// Response with the difference
    pub struct SubResponse {
        pub result: i64,
    }

    /// Request to multiply two numbers
    pub struct MulRequest {
        pub a: i64,
        pub b: i64,
    }

    /// Response with the product
    pub struct MulResponse {
        pub result: i64,
    }

    /// Request to divide two numbers
    pub struct DivRequest {
        pub a: i64,
        pub b: i64,
    }

    /// Response with the quotient (None if division by zero)
    pub struct DivResponse {
        pub result: Option<i64>,
    }
}

// ============================================================================
// Interface Definition - using rpc_interface! macro
// ============================================================================

rpc_interface! {
    /// Calculator service with basic arithmetic operations
    pub Calculator(0xCA1C_0000) {
        /// Addition operation
        add: 0,
        /// Subtraction operation
        sub: 1,
        /// Multiplication operation
        mul: 2,
        /// Division operation
        div: 3,
    }
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

    // Build transport using NetTransportBuilder
    let transport = NetTransportBuilder::new(network, time, task)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Server listening on {}\n", SERVER_ADDR);

    // Register handlers using interface tokens from rpc_interface! macro
    let (add_stream, _) =
        transport.register_handler_at::<AddRequest, _>(Calculator::BASE, 0, JsonCodec);
    let (sub_stream, _) =
        transport.register_handler_at::<SubRequest, _>(Calculator::BASE, 1, JsonCodec);
    let (mul_stream, _) =
        transport.register_handler_at::<MulRequest, _>(Calculator::BASE, 2, JsonCodec);
    let (div_stream, _) =
        transport.register_handler_at::<DivRequest, _>(Calculator::BASE, 3, JsonCodec);

    println!("Registered 4 calculator methods (add, sub, mul, div)\n");
    println!("Waiting for requests...\n");

    // Handle requests using tokio::select!
    loop {
        tokio::select! {
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

    // Build transport
    let transport = NetTransportBuilder::new(network, time.clone(), task)
        .local_address(local_addr)
        .build_listening()
        .await?;

    println!("Client started, connecting to server at {}\n", SERVER_ADDR);

    // Create endpoints using rpc_interface! generated tokens
    let add_endpoint = Calculator::endpoint(&server_addr, Calculator::add);
    let sub_endpoint = Calculator::endpoint(&server_addr, Calculator::sub);
    let mul_endpoint = Calculator::endpoint(&server_addr, Calculator::mul);
    let div_endpoint = Calculator::endpoint(&server_addr, Calculator::div);

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

        // Use request builder with built-in timeout handling
        let result = match op {
            Operation::Add(a, b) => transport
                .request(&add_endpoint, AddRequest { a: *a, b: *b })
                .with_timeout(Duration::from_secs(5), &time)
                .send::<AddResponse, _>(JsonCodec)
                .await
                .ok()
                .map(|resp| format!("{}", resp.result)),

            Operation::Sub(a, b) => transport
                .request(&sub_endpoint, SubRequest { a: *a, b: *b })
                .with_timeout(Duration::from_secs(5), &time)
                .send::<SubResponse, _>(JsonCodec)
                .await
                .ok()
                .map(|resp| format!("{}", resp.result)),

            Operation::Mul(a, b) => transport
                .request(&mul_endpoint, MulRequest { a: *a, b: *b })
                .with_timeout(Duration::from_secs(5), &time)
                .send::<MulResponse, _>(JsonCodec)
                .await
                .ok()
                .map(|resp| format!("{}", resp.result)),

            Operation::Div(a, b) => transport
                .request(&div_endpoint, DivRequest { a: *a, b: *b })
                .with_timeout(Duration::from_secs(5), &time)
                .send::<DivResponse, _>(JsonCodec)
                .await
                .ok()
                .map(|resp| {
                    resp.result
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| "ERROR (division by zero)".to_string())
                }),
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
            println!("This example demonstrates the DX improvements:\n");
            println!("  - rpc_messages! macro (auto-derive Serialize/Deserialize)");
            println!("  - rpc_interface! macro (generate interface tokens)");
            println!("  - transport.call() (convenience method for RPC)");
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
