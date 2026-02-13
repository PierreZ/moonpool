//! Hyper HTTP Example: Testing HTTP/1.1 services in deterministic simulation.
//!
//! This example demonstrates that **unmodified hyper** can run over moonpool-sim's
//! simulated TCP network. The simulated network provides deterministic chaos testing
//! (connection faults, latency, bit flips) — all transparent to hyper.
//!
//! ## What this shows
//!
//! - An HTTP/1.1 server and client running entirely within simulation
//! - `SimTcpStream` bridged to hyper via `TokioIo` adapter (no Send required)
//! - Multiple request/response cycles over a single HTTP/1.1 keep-alive connection
//! - Error handling for simulation shutdown and network faults
//!
//! ## How it works
//!
//! ```text
//! Client workload                          Server workload
//! ──────────────                           ──────────────
//! network.connect(server_ip)               network.bind(my_ip) + accept()
//!        │                                        │
//!        ▼                                        ▼
//! TokioIo<SimTcpStream>                   TokioIo<SimTcpStream>
//!        │                                        │
//!        ▼                                        ▼
//! hyper client::handshake()               hyper server::serve_connection()
//!        │                                        │
//!        ▼                                        ▼
//! sender.send_request(GET /hello)  ──►    service_fn(handle_request)
//! sender.send_request(POST /echo)  ──►    service_fn(handle_request)
//! ```
//!
//! ## Running
//!
//! ```bash
//! nix develop --command cargo run --example hyper_http -p moonpool-sim
//! ```

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

use moonpool_sim::{NetworkProvider, SimulationBuilder, SimulationError, TcpListenerTrait};

// ============================================================================
// HTTP Request Handler
// ============================================================================

/// Handle incoming HTTP requests.
///
/// Routes:
/// - `GET /hello` → "Hello from moonpool-sim!"
/// - `POST /echo` → echoes the request body
/// - anything else → 404 Not Found
async fn handle_request(
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Box<dyn std::error::Error + Send + Sync>> {
    let (parts, body) = req.into_parts();

    match (parts.method.as_str(), parts.uri.path()) {
        ("GET", "/hello") => Ok(Response::new(Full::new(Bytes::from(
            "Hello from moonpool-sim!",
        )))),
        ("POST", "/echo") => {
            let body_bytes = body.collect().await?.to_bytes();
            Ok(Response::new(Full::new(body_bytes)))
        }
        _ => {
            let mut resp = Response::new(Full::new(Bytes::from("Not Found")));
            *resp.status_mut() = StatusCode::NOT_FOUND;
            Ok(resp)
        }
    }
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("Failed to create Tokio LocalRuntime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload_fn("server", |ctx| {
                let network = ctx.network().clone();
                let my_ip = ctx.my_ip().to_string();
                async move {
                    let listener = network
                        .bind(&my_ip)
                        .await
                        .map_err(|e| SimulationError::InvalidState(format!("bind: {e}")))?;

                    // Accept one connection from the client
                    let (stream, _addr) = listener
                        .accept()
                        .await
                        .map_err(|e| SimulationError::InvalidState(format!("accept: {e}")))?;
                    let io = TokioIo::new(stream);

                    // Serve HTTP/1.1 on this connection until the client closes it
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service_fn(handle_request))
                        .await
                        .map_err(|e| {
                            SimulationError::InvalidState(format!("hyper server error: {e}"))
                        })?;

                    Ok(())
                }
            })
            .workload_fn("client", |ctx| {
                let network = ctx.network().clone();
                let peer_ip = ctx.peer().to_string();
                async move {
                    let stream = network
                        .connect(&peer_ip)
                        .await
                        .map_err(|e| SimulationError::InvalidState(format!("connect: {e}")))?;
                    let io = TokioIo::new(stream);

                    // HTTP/1.1 handshake
                    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
                        .await
                        .map_err(|e| {
                            SimulationError::InvalidState(format!("hyper handshake: {e}"))
                        })?;

                    // Drive the connection in the background
                    tokio::task::spawn_local(async move {
                        if let Err(e) = conn.await {
                            eprintln!("Connection driver error: {e}");
                        }
                    });

                    // --- Request 1: GET /hello ---
                    let req = Request::builder()
                        .uri("/hello")
                        .header("host", peer_ip.as_str())
                        .body(Full::new(Bytes::new()))
                        .map_err(|e| {
                            SimulationError::InvalidState(format!("request build: {e}"))
                        })?;

                    let res = sender
                        .send_request(req)
                        .await
                        .map_err(|e| SimulationError::InvalidState(format!("send_request: {e}")))?;

                    assert_eq!(res.status(), StatusCode::OK);
                    let body = res
                        .into_body()
                        .collect()
                        .await
                        .map_err(|e| SimulationError::InvalidState(format!("body collect: {e}")))?
                        .to_bytes();
                    assert_eq!(&body[..], b"Hello from moonpool-sim!");

                    // --- Request 2: POST /echo ---
                    let req = Request::builder()
                        .method("POST")
                        .uri("/echo")
                        .header("host", peer_ip.as_str())
                        .body(Full::new(Bytes::from("ping")))
                        .map_err(|e| {
                            SimulationError::InvalidState(format!("request build: {e}"))
                        })?;

                    let res = sender
                        .send_request(req)
                        .await
                        .map_err(|e| SimulationError::InvalidState(format!("send_request: {e}")))?;

                    assert_eq!(res.status(), StatusCode::OK);
                    let body = res
                        .into_body()
                        .collect()
                        .await
                        .map_err(|e| SimulationError::InvalidState(format!("body collect: {e}")))?
                        .to_bytes();
                    assert_eq!(&body[..], b"ping");

                    // --- Request 3: GET /nonexistent → 404 ---
                    let req = Request::builder()
                        .uri("/nonexistent")
                        .header("host", peer_ip.as_str())
                        .body(Full::new(Bytes::new()))
                        .map_err(|e| {
                            SimulationError::InvalidState(format!("request build: {e}"))
                        })?;

                    let res = sender
                        .send_request(req)
                        .await
                        .map_err(|e| SimulationError::InvalidState(format!("send_request: {e}")))?;

                    assert_eq!(res.status(), StatusCode::NOT_FOUND);

                    Ok(())
                }
            })
            .set_iterations(3)
            .run()
            .await
    });

    println!("Simulation Report:");
    println!("  Iterations:  {}", report.iterations);
    println!("  Successful:  {}", report.successful_runs);
    println!("  Failed:      {}", report.failed_runs);
    println!(
        "  Seeds used:  {:?}",
        &report.seeds_used[..report.seeds_used.len().min(10)]
    );

    if !report.seeds_failing.is_empty() {
        println!("  FAILING seeds: {:?}", report.seeds_failing);
        std::process::exit(1);
    }

    println!("\nAll iterations passed!");
}
