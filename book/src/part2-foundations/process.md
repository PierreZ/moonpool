# Process: Your Server

<!-- toc -->

- The `Process` trait: `name()` and `async fn run()`
- Factory pattern: `SimulationBuilder` calls your factory to create fresh instances on each boot
- State persistence: a process loses all in-memory state on reboot — only storage survives
- IP addressing: processes get IPs `10.0.1.{1..N}`
- Tags: `.tags(&[("role", &["leader", "follower"])])` for role-based configuration
- Graceful vs crash shutdown: `RebootKind::Graceful` signals a cancellation token with a grace period
