# Common Pitfalls

<!-- toc -->

- Don't call `stop().await` in simulation workloads — the yield loop hangs because the sim orchestrator needs to step events. Use `drop(host)` instead.
- Storage step-loop pattern is required: storage operations return `Poll::Pending`
- `yield_now()` is essential: spawned tasks need it to make progress
- No `unwrap()`: use `Result<T, E>` with `?` everywhere
- No direct tokio calls: use providers
- No `LocalSet`: use `build_local()` only
- `#[async_trait(?Send)]` for all networking traits
- Borrow checker patterns in world.rs: extract values from connections before calling functions that take `&mut SimInner`
