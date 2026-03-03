# The Single-Core Constraint

<!-- toc -->

- No Send, no Sync, no thread-level non-determinism
- `tokio::runtime::Builder::new_current_thread().build_local()` — not LocalSet
- Why this enables perfect reproducibility: one thread = one execution order
- Trade-off: no parallelism in simulation (production code can still use multi-threaded tokio)
- `#[async_trait(?Send)]` for all networking traits
