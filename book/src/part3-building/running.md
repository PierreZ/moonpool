# Running and Observing

<!-- toc -->

- `cargo xtask sim run <name>` — the primary way to run simulations
- Reading the `SimulationReport`: iterations, successes, failures, metrics
- Understanding output: total messages, failed messages, connection failures, reboot events
- What "success" means: no always-assertion violations AND all sometimes-assertions fired
- When to use `cargo nextest run` vs `cargo xtask sim`
