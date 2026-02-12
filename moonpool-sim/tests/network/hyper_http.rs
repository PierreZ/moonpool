//! Test demonstrating HTTP/1.1 over moonpool-sim's simulated TCP using hyper.
//!
//! ## Why not reqwest?
//!
//! reqwest cannot be used directly with moonpool-sim because its TCP connector
//! is sealed (`pub(crate)`) and always opens real `tokio::net::TcpStream` connections.
//! There is no public API to inject a custom transport (PR #1786 was rejected).
//!
//! ## Why hyper works
//!
//! hyper 1.x accepts any IO type implementing `Read + Write + Unpin` with **no Send
//! bound**, making it fully compatible with moonpool-sim's `SimTcpStream`. The
//! `hyper_util::rt::TokioIo` adapter bridges tokio's `AsyncRead`/`AsyncWrite` to
//! hyper's `Read`/`Write` traits.
//!
//! ## SimSafeIo wrapper
//!
//! moonpool-sim's `close_connection` marks **both** sides of a connection as closed
//! immediately when either side's stream is dropped. This creates a race condition
//! with hyper: when `serve_connection` returns, it drops the server's stream before
//! simulation events have delivered the response data to the client. The client then
//! sees a closed connection with no data (EOF) instead of the response.
//!
//! `SimSafeIo` prevents this by suppressing the destructor (via `ManuallyDrop`) and
//! no-oping `poll_shutdown`. The client side controls the connection lifecycle.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use moonpool_sim::{
    ChaosConfiguration, NetworkConfiguration, NetworkProvider, SimWorld, TcpListenerTrait,
};
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// ---------------------------------------------------------------------------
// SimSafeIo: prevents server-side stream drop from closing both endpoints
// ---------------------------------------------------------------------------

/// IO wrapper that suppresses the inner stream's destructor.
///
/// In simulation, `SimTcpStream::drop` calls `close_connection` which marks
/// **both** endpoints as closed immediately. When hyper's `serve_connection`
/// returns, it drops the server IO before events have delivered the response
/// data to the client. `SimSafeIo` prevents this race by leaking the stream
/// on drop and no-oping `poll_shutdown`.
struct SimSafeIo<T>(ManuallyDrop<T>);

impl<T> SimSafeIo<T> {
    fn new(inner: T) -> Self {
        Self(ManuallyDrop::new(inner))
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for SimSafeIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for SimSafeIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut *self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // No-op: prevent hyper's graceful shutdown from triggering close_connection.
        // The client side will close the connection after reading the response.
        Poll::Ready(Ok(()))
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Simple HTTP handler that returns a greeting with the request path.
async fn hello_handler(
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Box<dyn std::error::Error + Send + Sync>> {
    let path = req.uri().path().to_string();
    let body = format!("Hello from simulation! Path: {path}");
    Ok(Response::new(Full::new(Bytes::from(body))))
}

/// Drives the simulation event loop and spawned tasks to completion.
///
/// Interleaves sim event processing (delivers TCP data between endpoints) with
/// yielding to the tokio executor (lets hyper tasks read/write).
async fn drive_sim(
    sim: &mut SimWorld,
    handles: Vec<tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error>>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut iteration = 0u64;
    loop {
        let all_finished = handles.iter().all(|h| h.is_finished());
        if all_finished {
            break;
        }

        // Deliver TCP data through the simulation
        while sim.pending_event_count() > 0 {
            sim.step();
        }

        // Let tasks make progress
        tokio::task::yield_now().await;

        iteration += 1;
        if iteration > 100_000 {
            return Err("simulation did not converge after 100k iterations".into());
        }
    }

    for handle in handles {
        handle.await.map_err(|e| -> Box<dyn std::error::Error> {
            format!("task panicked: {e}").into()
        })??;
    }

    Ok(())
}

/// Build a deterministic, low-latency network config for tests.
fn test_network_config() -> NetworkConfiguration {
    NetworkConfiguration {
        bind_latency: Duration::from_micros(100)..Duration::from_micros(100),
        accept_latency: Duration::from_micros(100)..Duration::from_micros(100),
        connect_latency: Duration::from_micros(500)..Duration::from_micros(500),
        read_latency: Duration::from_micros(10)..Duration::from_micros(10),
        write_latency: Duration::from_micros(50)..Duration::from_micros(50),
        chaos: ChaosConfiguration::disabled(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_hyper_http_over_simulated_tcp() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async {
        let mut sim = SimWorld::new_with_network_config(test_network_config());
        let provider = sim.network_provider();
        let server_addr = "127.0.0.1:8080";

        // --- Server ---
        let sp = provider.clone();
        let server = tokio::task::spawn_local(async move {
            let listener = sp.bind(server_addr).await?;
            let (stream, _peer) = listener.accept().await?;
            let io = TokioIo::new(SimSafeIo::new(stream));

            hyper::server::conn::http1::Builder::new()
                .keep_alive(false)
                .serve_connection(io, service_fn(hello_handler))
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            Ok::<_, Box<dyn std::error::Error>>(())
        });

        // --- Client ---
        let cp = provider.clone();
        let client = tokio::task::spawn_local(async move {
            let stream = cp.connect(server_addr).await?;
            let io = TokioIo::new(stream);

            let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            tokio::task::spawn_local(async move {
                let _ = conn.await;
            });

            let req = Request::builder()
                .uri("/test")
                .header("connection", "close")
                .body(Full::new(Bytes::new()))
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            let res = sender
                .send_request(req)
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            assert_eq!(res.status(), StatusCode::OK);

            let body = res
                .into_body()
                .collect()
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
                .to_bytes();

            assert_eq!(body, "Hello from simulation! Path: /test");
            Ok::<_, Box<dyn std::error::Error>>(())
        });

        drive_sim(&mut sim, vec![server, client])
            .await
            .expect("HTTP simulation failed");

        assert!(
            sim.current_time() > Duration::ZERO,
            "Simulation time should have advanced"
        );
        println!(
            "HTTP over simulated TCP completed in {:?} sim time",
            sim.current_time()
        );
    });
}

#[test]
fn test_hyper_http_multiple_requests() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async {
        let mut sim = SimWorld::new_with_network_config(test_network_config());
        let provider = sim.network_provider();
        let server_addr = "127.0.0.1:9090";
        let num_requests: usize = 3;

        // --- Server: keep-alive, serves all requests on one connection ---
        let sp = provider.clone();
        let server = tokio::task::spawn_local(async move {
            let listener = sp.bind(server_addr).await?;
            let (stream, _peer) = listener.accept().await?;
            let io = TokioIo::new(SimSafeIo::new(stream));

            hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service_fn(hello_handler))
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            Ok::<_, Box<dyn std::error::Error>>(())
        });

        // --- Client: sends N requests, last one closes ---
        let cp = provider.clone();
        let client = tokio::task::spawn_local(async move {
            let stream = cp.connect(server_addr).await?;
            let io = TokioIo::new(stream);

            let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            tokio::task::spawn_local(async move {
                let _ = conn.await;
            });

            for i in 0..num_requests {
                let path = format!("/request/{i}");
                let mut builder = Request::builder().uri(&path);
                if i == num_requests - 1 {
                    builder = builder.header("connection", "close");
                }
                let req = builder
                    .body(Full::new(Bytes::new()))
                    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

                let res = sender
                    .send_request(req)
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                assert_eq!(res.status(), StatusCode::OK);

                let body = res
                    .into_body()
                    .collect()
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
                    .to_bytes();
                let expected = format!("Hello from simulation! Path: {path}");
                assert_eq!(body, expected.as_bytes());
            }

            Ok::<_, Box<dyn std::error::Error>>(())
        });

        drive_sim(&mut sim, vec![server, client])
            .await
            .expect("Multiple HTTP requests simulation failed");

        println!(
            "HTTP keep-alive with {} requests in {:?} sim time",
            num_requests,
            sim.current_time()
        );
    });
}

#[test]
fn test_hyper_http_with_request_body() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async {
        let mut sim = SimWorld::new_with_network_config(test_network_config());
        let provider = sim.network_provider();
        let server_addr = "127.0.0.1:7070";

        // --- Echo server ---
        let sp = provider.clone();
        let server = tokio::task::spawn_local(async move {
            let listener = sp.bind(server_addr).await?;
            let (stream, _peer) = listener.accept().await?;

            let echo = service_fn(|req: Request<Incoming>| async move {
                let body = req.into_body().collect().await?.to_bytes();
                let text = format!("Echo: {}", String::from_utf8_lossy(&body));
                Ok::<_, hyper::Error>(Response::new(Full::new(Bytes::from(text))))
            });

            let io = TokioIo::new(SimSafeIo::new(stream));
            hyper::server::conn::http1::Builder::new()
                .keep_alive(false)
                .serve_connection(io, echo)
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            Ok::<_, Box<dyn std::error::Error>>(())
        });

        // --- Client: POST with body ---
        let cp = provider.clone();
        let client = tokio::task::spawn_local(async move {
            let stream = cp.connect(server_addr).await?;
            let io = TokioIo::new(stream);

            let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            tokio::task::spawn_local(async move {
                let _ = conn.await;
            });

            let req = Request::builder()
                .method("POST")
                .uri("/echo")
                .header("connection", "close")
                .body(Full::new(Bytes::from("hello from the client")))
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            let res = sender
                .send_request(req)
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            assert_eq!(res.status(), StatusCode::OK);

            let body = res
                .into_body()
                .collect()
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
                .to_bytes();
            assert_eq!(body, "Echo: hello from the client");

            Ok::<_, Box<dyn std::error::Error>>(())
        });

        drive_sim(&mut sim, vec![server, client])
            .await
            .expect("HTTP echo simulation failed");

        println!(
            "HTTP POST echo completed in {:?} sim time",
            sim.current_time()
        );
    });
}

#[test]
fn test_hyper_http_with_network_latency() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async {
        let config = NetworkConfiguration {
            bind_latency: Duration::from_millis(1)..Duration::from_millis(1),
            accept_latency: Duration::from_millis(2)..Duration::from_millis(2),
            connect_latency: Duration::from_millis(5)..Duration::from_millis(5),
            read_latency: Duration::from_micros(100)..Duration::from_micros(100),
            write_latency: Duration::from_millis(1)..Duration::from_millis(1),
            chaos: ChaosConfiguration::disabled(),
        };
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();
        let server_addr = "10.0.0.1:80";

        let sp = provider.clone();
        let server = tokio::task::spawn_local(async move {
            let listener = sp.bind(server_addr).await?;
            let (stream, _peer) = listener.accept().await?;
            let io = TokioIo::new(SimSafeIo::new(stream));

            hyper::server::conn::http1::Builder::new()
                .keep_alive(false)
                .serve_connection(io, service_fn(hello_handler))
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            Ok::<_, Box<dyn std::error::Error>>(())
        });

        let cp = provider.clone();
        let client = tokio::task::spawn_local(async move {
            let stream = cp.connect(server_addr).await?;
            let io = TokioIo::new(stream);

            let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            tokio::task::spawn_local(async move {
                let _ = conn.await;
            });

            let req = Request::builder()
                .uri("/latency-test")
                .header("connection", "close")
                .body(Full::new(Bytes::new()))
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            let res = sender
                .send_request(req)
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            assert_eq!(res.status(), StatusCode::OK);

            let body = res
                .into_body()
                .collect()
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
                .to_bytes();
            assert_eq!(body, "Hello from simulation! Path: /latency-test");

            Ok::<_, Box<dyn std::error::Error>>(())
        });

        drive_sim(&mut sim, vec![server, client])
            .await
            .expect("HTTP latency simulation failed");

        let sim_time = sim.current_time();
        assert!(
            sim_time >= Duration::from_millis(5),
            "Expected at least 5ms with configured latencies, got {:?}",
            sim_time
        );
        println!(
            "HTTP over WAN-like latency completed in {:?} sim time",
            sim_time
        );
    });
}
