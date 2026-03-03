# Turn-Based Concurrency

<!-- toc -->

- The philosophy: one message at a time per identity
- No locks, no races, no shared mutable state between actor instances
- Per-identity mailboxes: each actor identity gets its own message queue
- Why this matters for simulation: eliminates concurrency bugs within an actor, focuses testing on inter-actor behavior
- Contrast with shared-state concurrency: actors trade throughput for correctness
