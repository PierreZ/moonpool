//! Hyper HTTP integration test: unmodified hyper over simulated TCP with chaos.
//!
//! Validates that HTTP/1.1 request/response cycles work correctly when run
//! entirely within moonpool-sim's deterministic simulation, including chaos
//! injection (connect failures, connection faults, latency).

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

use moonpool_sim::{
    NetworkProvider, SimContext, SimulationBuilder, SimulationReport, SimulationResult,
    TcpListenerTrait, Workload,
};

// ============================================================================
// Test Utilities
// ============================================================================

fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move { builder.run().await })
}

fn assert_simulation_success(report: &SimulationReport) {
    if !report.seeds_failing.is_empty() {
        panic!(
            "Simulation had {} failing seeds: {:?}",
            report.seeds_failing.len(),
            report.seeds_failing
        );
    }
    if !report.assertion_violations.is_empty() {
        panic!(
            "Assertion violations:\n{}",
            report
                .assertion_violations
                .iter()
                .map(|v| format!("  - {}", v))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

// ============================================================================
// HTTP Request Handler
// ============================================================================

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
// Server Workload
// ============================================================================

struct HyperServer;

#[async_trait(?Send)]
impl Workload for HyperServer {
    fn name(&self) -> &str {
        "server"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;

        let (stream, _addr) = tokio::select! {
            result = listener.accept() => result?,
            _ = ctx.shutdown().cancelled() => return Ok(()),
        };

        let io = TokioIo::new(stream);

        tokio::select! {
            result = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service_fn(handle_request)) => {
                // Ignore hyper errors during shutdown (connection reset, incomplete message)
                if let Err(e) = result {
                    tracing::debug!("hyper server error (expected under chaos): {e}");
                }
            }
            _ = ctx.shutdown().cancelled() => {}
        }

        Ok(())
    }
}

// ============================================================================
// Client Workload
// ============================================================================

struct HyperClient;

#[async_trait(?Send)]
impl Workload for HyperClient {
    fn name(&self) -> &str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let server_ip = ctx.peer("server").ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("server not found in peers".into())
        })?;

        // Connect with shutdown awareness (connect may hang forever under chaos)
        let stream = tokio::select! {
            result = ctx.network().connect(&server_ip) => result?,
            _ = ctx.shutdown().cancelled() => return Ok(()),
        };

        let io = TokioIo::new(stream);

        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("hyper handshake error: {e}"))
            })?;

        tokio::task::spawn_local(async move {
            if let Err(e) = conn.await {
                tracing::debug!("Connection driver error (expected under chaos): {e}");
            }
        });

        // Run all requests with shutdown awareness
        tokio::select! {
            result = send_requests(&mut sender, &server_ip) => result?,
            _ = ctx.shutdown().cancelled() => {}
        }

        Ok(())
    }
}

async fn send_requests(
    sender: &mut hyper::client::conn::http1::SendRequest<Full<Bytes>>,
    server_ip: &str,
) -> SimulationResult<()> {
    // GET /hello
    let req = Request::builder()
        .uri("/hello")
        .header("host", server_ip)
        .body(Full::new(Bytes::new()))
        .map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("request build error: {e}"))
        })?;

    let res = sender.send_request(req).await.map_err(|e| {
        moonpool_sim::SimulationError::InvalidState(format!("send_request error: {e}"))
    })?;

    assert_eq!(res.status(), StatusCode::OK);
    let body = res
        .into_body()
        .collect()
        .await
        .map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("body collect error: {e}"))
        })?
        .to_bytes();
    assert_eq!(&body[..], b"Hello from moonpool-sim!");

    // POST /echo
    let req = Request::builder()
        .method("POST")
        .uri("/echo")
        .header("host", server_ip)
        .body(Full::new(Bytes::from("ping")))
        .map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("request build error: {e}"))
        })?;

    let res = sender.send_request(req).await.map_err(|e| {
        moonpool_sim::SimulationError::InvalidState(format!("send_request error: {e}"))
    })?;

    assert_eq!(res.status(), StatusCode::OK);
    let body = res
        .into_body()
        .collect()
        .await
        .map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("body collect error: {e}"))
        })?
        .to_bytes();
    assert_eq!(&body[..], b"ping");

    // GET /nonexistent → 404
    let req = Request::builder()
        .uri("/nonexistent")
        .header("host", server_ip)
        .body(Full::new(Bytes::new()))
        .map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("request build error: {e}"))
        })?;

    let res = sender.send_request(req).await.map_err(|e| {
        moonpool_sim::SimulationError::InvalidState(format!("send_request error: {e}"))
    })?;

    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[test]
fn test_hyper_http_basic() {
    let report = run_simulation(
        SimulationBuilder::new()
            .workload(HyperServer)
            .workload(HyperClient)
            .set_iterations(3)
            .set_debug_seeds(vec![1, 2, 3]),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}
