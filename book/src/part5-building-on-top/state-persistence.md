# State Persistence

<!-- toc -->

- `ActorStateStore` trait: `load()`, `write()`, `clear()` with ETag optimistic concurrency
- `InMemoryStateStore`: HashMap-based, counter ETags
- `PersistentState<T>`: typed wrapper — `load()` → `T`, `write(&T)` → `()`
- Pattern: load state in `on_activate`, write state after mutation in `dispatch`
- ETag conflicts: detect concurrent modifications (important for distributed actors)
