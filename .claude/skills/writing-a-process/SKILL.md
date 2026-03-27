---
description: |
  Implementing a moonpool Process (system under test): trait, factory pattern, reboots, tags, providers.
  TRIGGER when: implementing the Process trait, creating a server/node for simulation, setting up process factories, or configuring process tags.
  DO NOT TRIGGER when: not working on moonpool simulation code.
---

# Writing a Process

## When to Use This Skill

Invoke when:
- Implementing a system under test (server, node, service)
- Creating a Process trait implementation
- Setting up the factory pattern for process reboots
- Configuring tags for role assignment

## Quick Reference

```rust
#[async_trait(?Send)]
pub trait Process: 'static {
    fn name(&self) -> &str;
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()>;
}
```

- **Factory**: `SimulationBuilder::new().processes(3, || Box::new(MyServer::new()))` — called once per process per boot
- **IPs**: Processes get `10.0.1.{1..N}`, workloads get `10.0.0.{1..N}`
- **Reboots**: All in-memory state is lost. Only storage-persisted data survives.
- **Graceful shutdown**: Check `ctx.shutdown().is_cancelled()` in your main loop
- **Tags**: `.tags(&[("role", &["leader", "follower"])])` — round-robin distribution
- **Providers**: Use `ctx.network()`, `ctx.time()`, `ctx.random()` — never call tokio directly

## Book Chapters

- `book/src/part2-foundations/08-process-workload.md` — process vs workload separation
- `book/src/part2-foundations/09-process.md` — full Process guide with examples
- `book/src/part3-building/02-defining-process.md` — step-by-step walkthrough
