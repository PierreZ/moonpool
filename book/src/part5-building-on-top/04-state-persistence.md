# State Persistence

<!-- toc -->

Virtual actors come and go. They activate on first message, sit idle, get deactivated to free memory, and re-activate later, possibly on a different node. If an actor's state only lives in memory, it disappears on deactivation. We need a persistence layer.

## ActorStateStore

The `ActorStateStore` trait is the actor system's storage abstraction. It provides three operations keyed by actor type name and identity string:

```rust
#[async_trait(?Send)]
pub trait ActorStateStore: fmt::Debug {
    async fn read_state(&self, actor_type: &str, actor_id: &str)
        -> Result<Option<StoredState>, ActorStateError>;

    async fn write_state(&self, actor_type: &str, actor_id: &str,
        data: Vec<u8>, expected_etag: Option<&str>)
        -> Result<String, ActorStateError>;

    async fn clear_state(&self, actor_type: &str, actor_id: &str,
        expected_etag: Option<&str>)
        -> Result<(), ActorStateError>;
}
```

State is stored as opaque bytes. The store does not know about your data types. Serialization is handled by the typed wrapper we will see next.

The `?Send` bound on `async_trait` matches moonpool's single-core execution model. No thread-safety overhead.

## ETags and Optimistic Concurrency

Every write returns a new ETag (a version string). Every subsequent write can pass the **expected** ETag. If the stored ETag does not match, the write fails with `ETagMismatch`:

```rust
let etag1 = store.write_state("BankAccount", "alice", data1, None).await?;     // first write, no etag
let etag2 = store.write_state("BankAccount", "alice", data2, Some(&etag1)).await?; // OK, etag matches
let _err  = store.write_state("BankAccount", "alice", data3, Some(&etag1)).await;  // FAIL: stale etag
```

Why does this matter? In a distributed actor system, two nodes might briefly both think they host the same actor (during a network partition or directory race). ETag concurrency ensures that the second writer detects the conflict rather than silently overwriting the first writer's state.

## InMemoryStateStore

For simulation and testing, moonpool provides `InMemoryStateStore`. It is a `HashMap` behind a `RefCell`, with ETags implemented as monotonically increasing counter values:

```rust
let state_store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());
```

No persistence across process restarts. This is fine for simulation, where the entire universe exists in a single process. For production, you would implement `ActorStateStore` against a real database.

## PersistentState\<T\>

Working directly with `ActorStateStore` means manual serialization and ETag tracking. `PersistentState<T>` wraps it into a typed, ergonomic API:

```rust
pub struct PersistentState<T> {
    value: T,
    etag: Option<String>,
    record_exists: bool,
    store: Rc<dyn ActorStateStore>,
    // ...
}
```

You create it with `load`, which reads existing state or creates a `T::default()`:

```rust
let ps = PersistentState::<BankAccountData>::load(
    store.clone(), "BankAccount", &ctx.id.identity
).await?;
```

Then access it with `state()` and `state_mut()`, and persist with `write_state()`:

```rust
ps.state_mut().balance += amount;
ps.write_state().await?;
```

The wrapper tracks the ETag internally. Each `write_state()` call sends the current ETag to the store and updates it on success. You never touch ETags manually.

## The Pattern: Load on Activate, Write on Mutate

The standard pattern ties persistence to the actor lifecycle:

```rust
#[derive(Default)]
struct BankAccountImpl {
    state: Option<PersistentState<BankAccountData>>,
}

// on_activate: load state from store
async fn on_activate<P: Providers, C: MessageCodec>(
    &mut self, ctx: &ActorContext<P, C>,
) -> Result<(), ActorError> {
    if let Some(store) = ctx.state_store() {
        self.state = Some(
            PersistentState::<BankAccountData>::load(
                store.clone(), "BankAccount", &ctx.id.identity,
            ).await.map_err(|e| ActorError::from(ActorHandlerError::HandlerError { message: format!("{e}") }))?
        );
    }
    Ok(())
}

// dispatch methods: mutate and write
async fn deposit<P: Providers, C: MessageCodec>(
    &mut self, ctx: &ActorContext<P, C>, req: DepositRequest,
) -> Result<BalanceResponse, RpcError> {
    if let Some(s) = &mut self.state {
        s.state_mut().balance += req.amount;
        let _ = s.write_state().await;
    }
    Ok(BalanceResponse { balance: self.balance() })
}
```

When the actor is deactivated (idle timeout, node shutdown), the state is already persisted from the last mutation. When it re-activates, `on_activate` loads the latest state from the store. The ETag ensures that if something went wrong (concurrent activation on two nodes), the conflict is detected rather than silently lost.

## Simulation Benefits

State persistence is especially valuable in simulation because we can test **crash recovery** scenarios:

- Kill a node mid-write. Does the actor recover correct state on re-activation?
- Force two nodes to activate the same actor. Does the ETag catch the conflict?
- Deactivate and re-activate rapidly. Is state preserved through the cycle?

The `InMemoryStateStore` is shared across all simulated nodes (via `Rc`), so all activations see the same data, exactly as they would with a real database. The simulation controls when crashes happen, making these scenarios reproducible and testable.
