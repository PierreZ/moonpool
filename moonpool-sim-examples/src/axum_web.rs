//! Axum web service simulation example.
//!
//! Demonstrates how to test an existing axum/hyper web service inside
//! moonpool-sim's deterministic simulation with chaos injection.
//!
//! The key insight: `SimTcpStream` implements `futures::io::AsyncRead + AsyncWrite`,
//! and `tokio_util::compat::Compat` bridges those to tokio's IO traits so hyper
//! (and therefore axum) works **unchanged** over simulated TCP.
//!
//! # Architecture
//!
//! - **Store trait**: dependency boundary for item persistence
//! - **`InMemoryStore`**: BTreeMap-based fake with buggify fault injection
//! - **`WebProcess`**: accepts TCP, serves axum via `hyper::serve_connection`
//! - **`WebWorkload`**: sends HTTP requests, validates responses under chaos

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use serde::{Deserialize, Serialize};
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::instrument;

use moonpool_sim::{
    NetworkProvider, Process, SimContext, SimulationResult, TcpListenerTrait, Workload,
};

// ============================================================================
// Domain types
// ============================================================================

/// An item in the store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Item {
    /// Unique identifier.
    pub id: u64,
    /// Item name.
    pub name: String,
}

/// Request body for creating an item.
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateItemRequest {
    /// Item name.
    pub name: String,
}

// ============================================================================
// Store trait — the dependency boundary
// ============================================================================

/// Trait for item persistence. In production, backed by a real database.
/// In simulation, backed by an in-memory `BTreeMap` with fault injection.
///
/// This is the "mock boundary": we simulate the network (HTTP traffic) via
/// moonpool, but fake the database at the service level. A fake with 80%
/// fidelity and deterministic fault injection beats a test container with
/// 100% fidelity but zero control over failure modes.
pub trait Store: Send + Sync + 'static {
    /// Create an item, returning its assigned ID.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::WriteFailed`] when the underlying write fails
    /// (modeled via buggify in the in-memory fake) or when an internal lock
    /// has been poisoned.
    fn create(&self, name: &str) -> Result<Item, StoreError>;

    /// Get an item by ID.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ReadFailed`] when the underlying read fails
    /// (modeled via buggify in the in-memory fake) or when an internal lock
    /// has been poisoned.
    fn get(&self, id: u64) -> Result<Option<Item>, StoreError>;
}

/// Store errors — designed with injectable failure modes in mind.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Simulated write failure (e.g., disk full, replication lag).
    #[error("write failed: {0}")]
    WriteFailed(String),

    /// Simulated read failure (e.g., connection pool exhaustion).
    #[error("read failed: {0}")]
    ReadFailed(String),
}

// ============================================================================
// InMemoryStore — fault-injectable fake
// ============================================================================

/// In-memory store backed by `BTreeMap` (deterministic iteration order).
///
/// Uses `buggify!()` to inject partial failures — the kind of failures a
/// real database container cannot produce. A test container fails as a whole
/// (binary up/down). This fake can fail individual writes while reads succeed,
/// or return stale data for specific IDs.
pub struct InMemoryStore {
    items: RwLock<BTreeMap<u64, Item>>,
    next_id: AtomicU64,
}

impl InMemoryStore {
    /// Create a new empty store.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            items: RwLock::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
        })
    }
}

impl Store for InMemoryStore {
    fn create(&self, name: &str) -> Result<Item, StoreError> {
        // Fault injection: randomly fail writes.
        // A real Postgres container can only be fully up or fully down.
        // This fake can fail individual writes — modeling disk full, replication
        // lag, or constraint violations that happen in production.
        if moonpool_sim::buggify!() {
            return Err(StoreError::WriteFailed("buggified write failure".into()));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let item = Item {
            id,
            name: name.to_string(),
        };
        let mut items = self
            .items
            .write()
            .map_err(|e| StoreError::WriteFailed(format!("lock poisoned: {e}")))?;
        items.insert(id, item.clone());
        Ok(item)
    }

    fn get(&self, id: u64) -> Result<Option<Item>, StoreError> {
        // Fault injection: randomly fail reads.
        // Models connection pool exhaustion or replica lag.
        if moonpool_sim::buggify_with_prob!(0.05) {
            return Err(StoreError::ReadFailed("buggified read failure".into()));
        }

        let items = self
            .items
            .read()
            .map_err(|e| StoreError::ReadFailed(format!("lock poisoned: {e}")))?;
        Ok(items.get(&id).cloned())
    }
}

// ============================================================================
// Axum handlers — standard axum, nothing moonpool-specific
// ============================================================================

#[instrument]
async fn health() -> &'static str {
    "ok"
}

#[instrument(skip(store))]
async fn create_item(
    State(store): State<Arc<dyn Store>>,
    Json(body): Json<CreateItemRequest>,
) -> impl IntoResponse {
    match store.create(&body.name) {
        Ok(item) => {
            tracing::info!(?item, "item created");
            (StatusCode::CREATED, Json(item)).into_response()
        }
        Err(e) => {
            tracing::warn!("create_item failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

#[instrument(skip(store))]
async fn get_item(State(store): State<Arc<dyn Store>>, Path(id): Path<u64>) -> impl IntoResponse {
    match store.get(id) {
        Ok(Some(item)) => {
            tracing::info!(?item, "item found");
            Json(item).into_response()
        }
        Ok(None) => {
            tracing::info!(id, "item not found");
            StatusCode::NOT_FOUND.into_response()
        }
        Err(e) => {
            tracing::warn!("get_item failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Build the axum router with the given store.
pub fn build_router(store: Arc<dyn Store>) -> axum::Router {
    axum::Router::new()
        .route("/health", get(health))
        .route("/items", post(create_item))
        .route("/items/{id}", get(get_item))
        .with_state(store)
}

// ============================================================================
// Process — the system under test
// ============================================================================

/// An axum web server running as a moonpool Process.
///
/// Uses `hyper::server::conn::http1::serve_connection` instead of `axum::serve`
/// because `axum::serve` requires `tokio::net::TcpListener`. We need moonpool's
/// simulated listener.
pub struct WebProcess;

#[async_trait]
impl Process for WebProcess {
    fn name(&self) -> &'static str {
        "web"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        use futures::stream::{FuturesUnordered, StreamExt};

        let store = InMemoryStore::new();
        let app = build_router(store);

        let listener = ctx.network().bind(ctx.my_ip()).await?;
        tracing::info!("server bound and listening");

        // hyper's serve_connection future is !Send (HTTP1 state machine holds
        // internal Rc<…>), so it cannot be spawned on the Send-bounded sim
        // runtime. Drive multiple in-flight connections inline via
        // FuturesUnordered instead.
        let mut connections = FuturesUnordered::new();

        loop {
            // Deliberately NOT `biased;`: accept (new connection) and
            // connections.next() (progress on in-flight connections) are peer
            // data sources; the seeded start offset lets different seeds
            // explore both orderings when both are ready.
            moonpool_sim::select! {
                accept = listener.accept() => {
                    let (stream, addr) = accept?;
                    tracing::info!(%addr, "accepted connection");

                    // SimTcpStream implements futures::io traits; .compat() bridges
                    // them back to tokio::io so TokioIo (and therefore hyper)
                    // accepts the stream.
                    let io = TokioIo::new(stream.compat());
                    let service = TowerToHyperService::new(app.clone());

                    connections.push(async move {
                        tracing::info!("serve_connection starting");
                        if let Err(e) = hyper::server::conn::http1::Builder::new()
                            .serve_connection(io, service)
                            .await
                        {
                            tracing::warn!(
                                "hyper serve_connection error (expected under chaos): {e}"
                            );
                        }
                        tracing::info!("serve_connection finished");
                    });
                }
                Some(()) = connections.next(), if !connections.is_empty() => {}
                () = ctx.shutdown().cancelled() => {
                    tracing::info!("server shutting down");
                    return Ok(());
                }
            }
        }
    }
}

// ============================================================================
// Workload — the test driver
// ============================================================================

/// Test driver that sends HTTP requests to the web process and validates responses.
pub struct WebWorkload;

#[async_trait]
impl Workload for WebWorkload {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let server_ip = ctx.peer("web").ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("web process not found".into())
        })?;

        tracing::info!(%server_ip, "workload starting");

        // Multiple rounds of requests to exercise chaos.
        // Each round is wrapped in a shutdown-aware select so the workload
        // exits cleanly when the orchestrator triggers shutdown (e.g., after
        // a chaos-induced connect hang is detected as no-progress).
        for round in 0..5 {
            tracing::info!(round, "starting round");
            let result = moonpool_sim::select! {
                biased;
                result = self.send_round(ctx, &server_ip, round) => result,
                () = ctx.shutdown().cancelled() => {
                    tracing::info!(round, "shutdown during round, exiting");
                    break;
                }
            };
            match result {
                Ok(()) => {
                    tracing::info!(round, "round completed successfully");
                }
                Err(e) => {
                    // Under chaos (connection drops, process reboots), requests
                    // can fail. That's expected — we're testing resilience.
                    moonpool_sim::assert_sometimes!(true, "request_round_failed");
                    tracing::warn!(round, "round failed (expected under chaos): {e}");
                }
            }
        }

        tracing::info!("workload finished all rounds");
        Ok(())
    }
}

impl WebWorkload {
    async fn send_round(
        &self,
        ctx: &SimContext,
        server_ip: &str,
        round: u32,
    ) -> SimulationResult<()> {
        tracing::info!(round, "connecting to server");
        let stream = moonpool_sim::select! {
            biased;
            result = ctx.network().connect(server_ip) => result?,
            () = ctx.shutdown().cancelled() => return Ok(()),
        };
        tracing::info!(round, "connected, starting handshake");

        let io = TokioIo::new(stream.compat());
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("handshake: {e}")))?;

        tracing::info!(round, "handshake complete, driving conn inline");
        let driver = async move {
            tracing::info!("client conn driver starting");
            if let Err(e) = conn.await {
                tracing::warn!("client conn driver error: {e}");
            }
            tracing::info!("client conn driver finished");
        };

        // hyper's SendRequest only makes progress while the Connection future
        // is polled, so the driver MUST be raced alongside the requests (a
        // merely pinned-but-never-polled driver leaves every request pending
        // forever). Same idiom as the hyper_http integration test.
        moonpool_sim::select! {
            biased;
            result = async {
                Self::check_health(&mut sender, server_ip, round).await?;
                Self::create_and_read_item(&mut sender, server_ip, round).await?;
                Self::check_not_found(&mut sender, server_ip, round).await?;
                Ok::<(), moonpool_sim::SimulationError>(())
            } => result?,
            // Connection died first (expected under chaos): give up the round.
            () = driver => {}
        }

        Ok(())
    }

    async fn check_health(
        sender: &mut hyper::client::conn::http1::SendRequest<Full<Bytes>>,
        server_ip: &str,
        round: u32,
    ) -> SimulationResult<()> {
        tracing::info!(round, "sending GET /health");
        let req = Request::builder()
            .uri("/health")
            .header("host", server_ip)
            .body(Full::new(Bytes::new()))
            .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("build: {e}")))?;

        let res = sender
            .send_request(req)
            .await
            .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("health: {e}")))?;
        tracing::info!(round, status = %res.status(), "GET /health response");
        moonpool_sim::assert_always!(
            res.status() == StatusCode::OK,
            "health endpoint must return 200"
        );
        Ok(())
    }

    async fn create_and_read_item(
        sender: &mut hyper::client::conn::http1::SendRequest<Full<Bytes>>,
        server_ip: &str,
        round: u32,
    ) -> SimulationResult<()> {
        tracing::info!(round, "sending POST /items");
        let body = serde_json::to_vec(&CreateItemRequest {
            name: "test-item".to_string(),
        })
        .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("serialize: {e}")))?;

        let req = Request::builder()
            .method("POST")
            .uri("/items")
            .header("host", server_ip)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("build: {e}")))?;

        let res = sender
            .send_request(req)
            .await
            .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("create: {e}")))?;

        let status = res.status();
        tracing::info!(round, %status, "POST /items response");
        let body_bytes = res
            .into_body()
            .collect()
            .await
            .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("body: {e}")))?
            .to_bytes();

        if status == StatusCode::CREATED {
            moonpool_sim::assert_sometimes!(true, "item_created_successfully");

            let item: Item = serde_json::from_slice(&body_bytes).map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("deserialize: {e}"))
            })?;

            Self::read_item_back(sender, server_ip, round, &item).await?;
        } else if status == StatusCode::INTERNAL_SERVER_ERROR {
            // Store write failure via buggify — expected
            moonpool_sim::assert_sometimes!(true, "store_write_failed");
        } else {
            moonpool_sim::assert_always!(false, format!("unexpected POST status: {status}"));
        }
        Ok(())
    }

    async fn read_item_back(
        sender: &mut hyper::client::conn::http1::SendRequest<Full<Bytes>>,
        server_ip: &str,
        round: u32,
        item: &Item,
    ) -> SimulationResult<()> {
        tracing::info!(round, item_id = item.id, "sending GET /items/{}", item.id);
        let req = Request::builder()
            .uri(format!("/items/{}", item.id))
            .header("host", server_ip)
            .body(Full::new(Bytes::new()))
            .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("build: {e}")))?;

        let res = sender
            .send_request(req)
            .await
            .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("get: {e}")))?;

        let get_status = res.status();
        tracing::info!(round, %get_status, "GET /items/{} response", item.id);
        if get_status == StatusCode::OK {
            let get_body = res
                .into_body()
                .collect()
                .await
                .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("body: {e}")))?
                .to_bytes();

            let fetched: Item = serde_json::from_slice(&get_body).map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("deserialize: {e}"))
            })?;

            // What we wrote must match what we read.
            moonpool_sim::assert_always!(
                fetched.id == item.id && fetched.name == item.name,
                "read-after-write consistency"
            );
        } else if get_status == StatusCode::INTERNAL_SERVER_ERROR {
            // Store read failure via buggify — expected
            moonpool_sim::assert_sometimes!(true, "store_read_failed");
        } else {
            moonpool_sim::assert_always!(false, format!("unexpected GET status: {get_status}"));
        }
        Ok(())
    }

    async fn check_not_found(
        sender: &mut hyper::client::conn::http1::SendRequest<Full<Bytes>>,
        server_ip: &str,
        round: u32,
    ) -> SimulationResult<()> {
        tracing::info!(round, "sending GET /items/999999");
        let req = Request::builder()
            .uri("/items/999999")
            .header("host", server_ip)
            .body(Full::new(Bytes::new()))
            .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("build: {e}")))?;

        let res = sender
            .send_request(req)
            .await
            .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("get-404: {e}")))?;

        // May get 404 (normal) or 500 (buggified read failure)
        let not_found_status = res.status();
        tracing::info!(round, %not_found_status, "GET /items/999999 response");
        moonpool_sim::assert_always!(
            not_found_status == StatusCode::NOT_FOUND
                || not_found_status == StatusCode::INTERNAL_SERVER_ERROR,
            "nonexistent item must return 404 or 500"
        );
        Ok(())
    }
}
