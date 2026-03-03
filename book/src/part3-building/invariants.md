# System Invariants

<!-- toc -->

- The `InvariantCheck` trait: validates cross-workload properties after every simulation event
- Different from assertion macros: invariants are global, run frequently, panic on violation
- Actors expose state via JSON through `StateRegistry`
- Example: "total money in the system is conserved" across all bank account actors
- When to use invariants vs assertions: invariants for global cross-actor properties, assertions for per-workload validation
- Performance consideration: invariants run after every event — keep them fast
