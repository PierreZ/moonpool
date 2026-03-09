# Wiring a Web Service
<!-- toc -->

Theory is cheap. Here's a complete worked example: an axum web service running inside moonpool-sim with chaos injection, fault-injectable storage, and assertion-based validation. The full source lives in `moonpool-sim-examples/src/axum_web.rs`.

## Step 1: The Store Trait

Every dependency boundary starts with a trait. This one models item persistence:

```rust
pub trait Store: Send + Sync + 'static {
    fn create(&self, name: &str) -> Result<Item, StoreError>;
    fn get(&self, id: u64) -> Result<Option<Item>, StoreError>;
}
```

`Send + Sync + 'static` because axum requires `State` to be `Send + Sync`. In production, this trait is backed by Postgres or SQLite. In simulation, it's backed by a BTreeMap.

Notice these are synchronous methods. The real database calls would be async, but for a fake that never does I/O, synchronous is simpler and equally correct. If your production trait has async methods, that works too.

## Step 2: The InMemoryStore

BTreeMap for deterministic ordering. `AtomicU64` for ID generation. `RwLock` because axum needs `Send + Sync`.

```rust
pub struct InMemoryStore {
    items: RwLock<BTreeMap<u64, Item>>,
    next_id: AtomicU64,
}

impl Store for InMemoryStore {
    fn create(&self, name: &str) -> Result<Item, StoreError> {
        // Fault injection: randomly fail writes.
        // Models disk full, replication lag, constraint violations.
        if buggify!() {
            return Err(StoreError::WriteFailed("buggified".into()));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let item = Item { id, name: name.to_string() };
        self.items.write()
            .map_err(|e| StoreError::WriteFailed(format!("{e}")))?
            .insert(id, item.clone());
        Ok(item)
    }

    fn get(&self, id: u64) -> Result<Option<Item>, StoreError> {
        // Lower probability: reads fail less often than writes in practice.
        if buggify_with_prob!(0.05) {
            return Err(StoreError::ReadFailed("buggified".into()));
        }

        Ok(self.items.read()
            .map_err(|e| StoreError::ReadFailed(format!("{e}")))?
            .get(&id).cloned())
    }
}
```

The `buggify!()` calls are the whole point. A Postgres container is either up or down. This fake can fail a write while the next read succeeds. It can fail creates at 25% while gets fail at 5%, modeling asymmetric failure that actually happens in production.

## Step 3: The Axum Router

Standard axum. Nothing moonpool-specific here:

```rust
pub fn build_router(store: Arc<dyn Store>) -> axum::Router {
    axum::Router::new()
        .route("/health", get(health))
        .route("/items", post(create_item))
        .route("/items/{id}", get(get_item))
        .with_state(store)
}
```

The handlers use `State(store): State<Arc<dyn Store>>` and return standard axum responses. `create_item` returns 201 on success, 500 when the store fails. `get_item` returns 200, 404, or 500. If you already have an axum app, your existing router works here.

## Step 4: The Process

This is where moonpool enters the picture. A `Process` is the system under test, running on a simulated server node:

```rust
#[async_trait(?Send)]
impl Process for WebProcess {
    fn name(&self) -> &str { "web" }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let store = InMemoryStore::new();
        let app = build_router(store);
        let listener = ctx.network().bind(ctx.my_ip()).await?;

        loop {
            let (stream, _addr) = tokio::select! {
                result = listener.accept() => result?,
                _ = ctx.shutdown().cancelled() => return Ok(()),
            };

            let io = TokioIo::new(stream);
            // TowerToHyperService bridges axum's tower::Service to hyper's Service
            let service = TowerToHyperService::new(app.clone());

            // spawn_local, not spawn: the future holds !Send SimTcpStream.
            // Axum handlers ARE Send (axum's requirement), but hyper polls
            // them inline within the connection future. Both coexist correctly.
            tokio::task::spawn_local(async move {
                if let Err(e) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await
                {
                    tracing::debug!("hyper error (expected under chaos): {e}");
                }
            });
        }
    }
}
```

Two things to note. First, we use `hyper::server::conn::http1::serve_connection`, **not** `axum::serve()`. `axum::serve` takes `tokio::net::TcpListener` directly, so it can't accept our simulated listener. `serve_connection` takes any `AsyncRead + AsyncWrite`, which `SimTcpStream` satisfies through the `TokioIo` adapter.

Second, `spawn_local` instead of `spawn`. The future holds a `SimTcpStream` which is `!Send`. Axum handlers remain `Send` (axum enforces this at compile time). The two coexist because hyper polls handlers inline within the connection future. The handler never escapes to another thread. This is architecturally correct, not a workaround.

## Step 5: The Workload

The workload is the test driver. It connects to the process, sends requests, and validates responses:

```rust
#[async_trait(?Send)]
impl Workload for WebWorkload {
    fn name(&self) -> &str { "client" }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let server_ip = ctx.peer("web").ok_or_else(|| {
            SimulationError::InvalidState("web process not found".into())
        })?;

        for round in 0..5 {
            match self.send_round(ctx, &server_ip, round).await {
                Ok(()) => {}
                Err(e) => {
                    // Under chaos, requests can fail. That's expected.
                    assert_sometimes!(true, "request_round_failed");
                    tracing::debug!("round {round} failed: {e}");
                }
            }
        }
        Ok(())
    }
}
```

Inside `send_round`, the workload creates a hyper client connection, sends requests, and uses assertions to validate behavior:

- `assert_always!` for invariants: health returns 200, read-after-write returns the same data, nonexistent items return 404 or 500.
- `assert_sometimes!` for coverage: items sometimes created successfully, store reads sometimes fail, request rounds sometimes fail under chaos.

The `assert_sometimes!` calls are how moonpool knows it's actually exercising error paths. If `store_write_failed` never triggers across thousands of iterations, something is wrong with the chaos configuration.

## Step 6: Wire It Together

```rust
SimulationBuilder::new()
    .processes(1, || Box::new(WebProcess))
    .workload(WebWorkload)
    .set_iterations(10)
    .run();
```

One web server process, one workload driving requests, ten iterations with different seeds. Each iteration creates a fresh simulation: new network, new processes, new store state, new buggify activation decisions.

The default network configuration injects latency and connection faults. Combined with `buggify!()` in the store, your handlers face both network-level chaos (connection drops, latency spikes) and application-level chaos (write failures, stale reads) deterministically and reproducibly.

When a seed fails, you replay it with `set_debug_seeds(vec![failing_seed])` and `set_iterations(1)` to reproduce the exact sequence of events that triggered the bug.
