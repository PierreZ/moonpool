# ActorHandler and Lifecycle

<!-- toc -->

- The `ActorHandler` trait: `dispatch()`, `on_activate()`, `on_deactivate()`
- `DeactivationHint`: `KeepAlive` (default) vs `DeactivateOnIdle` (deactivate after each dispatch)
- Activation: first message to an identity triggers activation
- Deactivation: stream close, idle timeout, or explicit hint
- `ActorHost`: server-side runtime — `register::<MyActor>()`, `stop().await`
- Routing loop + per-identity processing loop architecture
