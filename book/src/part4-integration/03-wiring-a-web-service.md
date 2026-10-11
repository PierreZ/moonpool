# Wiring a Web Service
<!-- toc -->

Theory is cheap. Here is a complete worked example: an axum web service inside
moonpool-sim, with a fault-injectable store, network chaos and assertions. The
full source is
[`crates/moonpool-sim-examples/src/axum_web.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool-sim-examples/src/axum_web.rs),
and `cargo xtask sim run axum` runs it.

## Step 1: The Store Trait

Every dependency boundary starts with a trait. This one models item
persistence:

```rust
pub trait Store: Send + Sync + 'static {
    fn create(&self, name: &str) -> Result<Item, StoreError>;
    fn get(&self, id: u64) -> Result<Option<Item>, StoreError>;
}
```

`Send + Sync + 'static` because axum requires `State` to be `Send + Sync`. In
production, Postgres or SQLite implements this trait. In simulation, a
`BTreeMap` does.

The methods are synchronous. A fake that never does I/O has no reason to be
async. If your production trait is async, that works too.

## Step 2: The InMemoryStore

`BTreeMap` for deterministic iteration order. `AtomicU64` for ID generation.
`RwLock` because axum needs `Send + Sync`.

```rust
pub struct InMemoryStore {
    items: RwLock<BTreeMap<u64, Item>>,
    next_id: AtomicU64,
}

impl Store for InMemoryStore {
    fn create(&self, name: &str) -> Result<Item, StoreError> {
        // Models disk full, replication lag, constraint violations.
        // A Postgres container cannot fail one write and pass the next.
        if moonpool_sim::buggify!() {
            return Err(StoreError::WriteFailed("buggified write failure".into()));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let item = Item { id, name: name.to_string() };
        let mut items = self.items.write()
            .expect("RwLock poisoned: prior task panicked");
        items.insert(id, item.clone());
        Ok(item)
    }

    fn get(&self, id: u64) -> Result<Option<Item>, StoreError> {
        // Reads fail less often than writes in practice.
        if moonpool_sim::buggify_with_prob!(0.05) {
            return Err(StoreError::ReadFailed("buggified read failure".into()));
        }
        let items = self.items.read()
            .expect("RwLock poisoned: prior task panicked");
        Ok(items.get(&id).cloned())
    }
}
```

The buggify sites are the whole point. The fake fails creates at 25% and gets
at 5%, once the site is active for the seed. That is the asymmetric failure a
real database shows and a container cannot.

## Step 3: The Axum Router

Standard axum. Nothing here is moonpool-specific:

```rust
pub fn build_router(store: Arc<dyn Store>) -> axum::Router {
    axum::Router::new()
        .route("/health", get(health))
        .route("/items", post(create_item))
        .route("/items/{id}", get(get_item))
        .with_state(store)
}
```

The handlers take `State(store): State<Arc<dyn Store>>` and return ordinary
axum responses. `create_item` returns 201 on success and 500 when the store
fails. `get_item` returns 200, 404 or 500. If you already have an axum app,
your router goes here unchanged.

## Step 4: The Process

A `Process` is the system under test. It runs on a simulated node:

```rust
#[async_trait]
impl Process for WebProcess {
    fn name(&self) -> &'static str { "web" }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        use futures::stream::{FuturesUnordered, StreamExt};

        let store = InMemoryStore::new();
        let app = build_router(store);
        let listener = ctx.network().bind(ctx.my_ip()).await?;

        // hyper's HTTP/1 serve_connection future is !Send, so no
        // Send-bounded spawn accepts it. Drive every in-flight
        // connection inline from one task.
        let mut connections = FuturesUnordered::new();

        loop {
            // Not `biased;`: a new accept and progress on an existing
            // connection are peers. The seed decides which wins.
            moonpool_sim::select! {
                accept = listener.accept() => {
                    let (stream, _addr) = accept?;

                    // Vectored writes on: the sim delivers each IoSlice
                    // separately, each with its own chance of chaos.
                    let io = HyperIo::new(stream).with_vectored_writes(true);
                    let service = TowerToHyperService::new(app.clone());

                    connections.push(async move {
                        if let Err(e) = hyper::server::conn::http1::Builder::new()
                            .serve_connection(io, service)
                            .await
                        {
                            tracing::warn!("hyper error (expected under chaos): {e}");
                        }
                    });
                }
                Some(()) = connections.next(), if !connections.is_empty() => {}
                () = ctx.shutdown().cancelled() => return Ok(()),
            }
        }
    }
}
```

Two points. First, the process calls
`hyper::server::conn::http1::Builder::serve_connection`, **not**
`axum::serve()`. `axum::serve` takes a `tokio::net::TcpListener`, so it cannot
accept the simulated listener. `serve_connection` takes anything that
implements hyper's IO traits. `HyperIo` from `moonpool-hyper` is that adapter,
and `TowerToHyperService` turns the axum router (a tower service) into the
hyper service the builder wants. The example needs neither `tokio-util` nor
`hyper-util`.

Second, **`FuturesUnordered` instead of spawning.** The blocker is hyper's
HTTP/1 state machine, which holds `Rc<…>` cells and so is `!Send`. It cannot
cross a Send-bounded spawn, but it polls fine from the accept loop. The
`select!` is moonpool's and stays in its default form on purpose. Accepting a
connection and progressing one are both ready in the same step, and different
seeds explore both orders.

## Step 5: The Workload

The workload is the test driver. It connects, sends requests and checks the
answers:

```rust
#[async_trait]
impl Workload for WebWorkload {
    fn name(&self) -> &'static str { "client" }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let server_ip = peer_ip(ctx, "web")?;

        for round in 0..5 {
            // Stop cleanly when the orchestrator ends the run mid-round.
            let Some(result) =
                unless_shutdown(ctx, self.send_round(ctx, &server_ip, round)).await
            else { break };

            if let Err(e) = result {
                // Connection drops and reboots are expected under chaos.
                moonpool_sim::assert_sometimes!(true, "request_round_failed");
                tracing::warn!(round, "round failed (expected under chaos): {e}");
            }
        }
        Ok(())
    }
}
```

`peer_ip` and `unless_shutdown` are small helpers from the examples crate.
`unless_shutdown` races a future against `ctx.shutdown().cancelled()` with a
`biased;` select, so it draws no scheduling randomness.

Inside `send_round`, the workload opens a hyper client connection. The client
has the same `!Send` problem: `hyper::client::conn::http1::handshake` returns a
`conn` driver future that must be polled, or every request stays pending
forever. The workload races the driver against the requests:

```rust
let io = HyperIo::new(stream).with_vectored_writes(true);
let (sender, conn) = hyper::client::conn::http1::handshake(io).await?;
let mut client = HttpClient { sender, host: server_ip };

moonpool_sim::select! {
    biased;
    result = async {
        Self::check_health(&mut client, round).await?;
        Self::create_and_read_item(&mut client, round).await?;
        Self::check_not_found(&mut client, round).await
    } => result?,
    // The connection died first: give up the round.
    () = conn => {}
}
```

The assertions split into two kinds:

- `assert_always!` for invariants: `/health` returns 200, a read after a
  successful write returns the same item, a missing item returns 404 or 500.
- `assert_sometimes!` for coverage: `item_created_successfully`,
  `store_write_failed`, `store_read_failed`, `request_round_failed`.

The `assert_sometimes!` calls are how moonpool knows the error paths ran. If
`store_write_failed` never fires across 50 seeds, the chaos configuration is
wrong, and the report says so.

## Step 6: Wire It Together

The `sim-axum-web` binary does the wiring:

```rust
let report = SimulationBuilder::new()
    .processes(1, || Box::new(WebProcess))
    .workload(WebWorkload)
    .set_iterations(50)
    .run()
    .expect("simulation configuration is valid");

finish_or_exit_on_failure(&report);
```

One web process, one workload, 50 seeds. Each iteration starts fresh: new
network, new process, new store, new buggify activation. The default network
configuration injects latency and connection faults, so your handlers face
network chaos and store chaos at the same time.

When a seed fails, replay it with `.set_debug_seeds(vec![seed])` and
`.set_iterations(1)`. [Reproducing with FixedCount](../part3-building/21-reproducing.md)
shows the full loop.

## Going Further

This example runs HTTP/1 by hand. For HTTP/2 and gRPC, `moonpool-hyper`
supplies the executor and timer hyper needs, plus a reconnecting client channel
and a serve helper. [The hyper Stack](./08-hyper-stack.md) picks up there.
