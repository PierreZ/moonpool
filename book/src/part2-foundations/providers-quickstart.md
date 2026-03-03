# Quick Start: Swapping Implementations

<!-- toc -->

- Code example: `TokioTimeProvider` vs sim time provider — same code, different behavior
- Show how a function generic over `TimeProvider` works in both contexts
- The forbidden list: no `tokio::time::sleep()`, no `tokio::spawn()`, no direct tokio calls
- Instead: `time.sleep()`, `time.timeout()`, `task_provider.spawn_task()`
